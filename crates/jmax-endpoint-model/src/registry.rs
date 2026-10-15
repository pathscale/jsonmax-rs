//! Validated, borrowed endpoint indexes and deterministic role authorization.
//!
//! Construction has no side effects on the model. All names are compared by
//! their exact bytes, except HTTP header names, which are ASCII case insensitive.
//! Type roots have depth zero; at most `MAX_REFERENCE_DEPTH` ownership or
//! reference edges may be traversed before reaching a terminal type.

use super::*;
use alloc::collections::BTreeSet;

/// Validate the complete model and construct indexes without copying its records.
///
/// Nominal declarations are collected before reference validation, so forward
/// references and recursive records are supported. No index escapes on failure.
pub fn build_registry(model: &EndpointModel) -> Result<Registry<'_>> {
    let mut registry = Registry {
        model,
        services_by_id: BTreeMap::new(),
        services_by_name: BTreeMap::new(),
        endpoints_by_code: BTreeMap::new(),
        endpoints_by_method: BTreeMap::new(),
        definitions_by_name: BTreeMap::new(),
        roles_by_name: BTreeMap::new(),
        errors_by_code: BTreeMap::new(),
    };

    for (service_index, service) in model.services.iter().enumerate() {
        identifier(&service.name)?;
        if registry.services_by_id.insert(service.id, service_index).is_some() {
            return Err(invalid("duplicate service id"));
        }
        if registry.services_by_name.insert(service.name.as_str(), service_index).is_some() {
            return Err(invalid("duplicate service name"));
        }
        let mut endpoint_names = BTreeSet::new();
        for (endpoint_index, endpoint) in service.endpoints.iter().enumerate() {
            identifier(&endpoint.name)?;
            if endpoint.service != service.name {
                return Err(invalid("endpoint service does not match its enclosing service"));
            }
            if !endpoint_names.insert(endpoint.name.as_str()) {
                return Err(invalid("duplicate endpoint name within service"));
            }
            let location = EndpointLocation { service_index, endpoint_index };
            if registry.endpoints_by_code.insert(endpoint.code, location).is_some() {
                return Err(invalid("duplicate endpoint code"));
            }
            // Both components have already passed the canonical identifier
            // grammar. This is precisely wire_names::method_name's spelling.
            let mut method = service.name.clone();
            method.push('.');
            method.push_str(&endpoint.name);
            if registry.endpoints_by_method.insert(method, location).is_some() {
                return Err(invalid("duplicate canonical endpoint method"));
            }
        }
    }

    let mut types = BTreeMap::new();
    for (index, definition) in model.definitions.iter().enumerate() {
        identifier(&definition.name)?;
        if registry.definitions_by_name.insert(definition.name.as_str(), index).is_some() {
            return Err(invalid("duplicate definition name"));
        }
        types.insert(definition.name.as_str(), &definition.ty);
    }
    for (index, role) in model.roles.iter().enumerate() {
        identifier(&role.name)?;
        if registry.roles_by_name.insert(role.name.as_str(), index).is_some() {
            return Err(invalid("duplicate role name"));
        }
    }
    let mut error_names = BTreeSet::new();
    for (index, error) in model.errors.iter().enumerate() {
        identifier(&error.name)?;
        if !error_names.insert(error.name.as_str()) {
            return Err(invalid("duplicate error name"));
        }
        if registry.errors_by_code.insert(error.code, index).is_some() {
            return Err(invalid("duplicate error code"));
        }
    }

    // Validate owned trees in declaration order. References are not expanded in
    // this pass; every inline nominal declaration is entered exactly once.
    for service in &model.services {
        for endpoint in &service.endpoints {
            collect_fields(&endpoint.params, 0, &mut types)?;
            collect_fields(&endpoint.response, 0, &mut types)?;
            if let Some(ty) = &endpoint.stream_response {
                collect_type(ty, 0, &mut types)?;
            }
        }
    }
    for definition in &model.definitions {
        if let EndpointType::Struct { name, .. } | EndpointType::Enum { name, .. } = &definition.ty {
            if name != &definition.name {
                return Err(invalid("definition name does not match its nominal type"));
            }
        }
        collect_type(&definition.ty, 0, &mut types)?;
    }
    let mut header_names = BTreeSet::new();
    for header in &model.headers {
        if !header_names.insert(header_key(&header.name)?) {
            return Err(invalid("duplicate header name"));
        }
        collect_type(&header.ty, 0, &mut types)?;
    }

    validate_roles(&registry)?;
    for service in &model.services {
        for endpoint in &service.endpoints {
            required_roles(&registry, &endpoint.roles)?;
        }
    }

    let mut graph = TypeGraph {
        types: &types,
        active: BTreeSet::new(),
        visited: BTreeMap::new(),
    };
    for service in &model.services {
        for endpoint in &service.endpoints {
            for field in endpoint.params.iter().chain(&endpoint.response) {
                graph.validate(&field.ty, 0)?;
            }
            if let Some(ty) = &endpoint.stream_response {
                graph.validate(ty, 0)?;
            }
        }
    }
    for definition in &model.definitions {
        graph.validate(&definition.ty, 0)?;
    }
    for header in &model.headers {
        graph.validate(&header.ty, 0)?;
    }
    Ok(registry)
}

/// Look up an exact service id, including zero and `u16::MAX`.
pub fn service_by_id<'a>(registry: &Registry<'a>, id: u16) -> Option<&'a ServiceSpec> {
    registry.model.services.get(*registry.services_by_id.get(&id)?)
}

/// Look up an exact, case-sensitive service name.
pub fn service_by_name<'a>(registry: &Registry<'a>, name: &str) -> Option<&'a ServiceSpec> {
    registry.model.services.get(*registry.services_by_name.get(name)?)
}

/// Endpoint codes are global across services and independent of error codes.
pub fn endpoint_by_code<'a>(registry: &Registry<'a>, code: u32) -> Option<&'a EndpointSchemaSpec> {
    endpoint_at(registry.model, *registry.endpoints_by_code.get(&code)?)
}

/// Look up the exact canonical `service.endpoint` method string.
pub fn endpoint_by_method<'a>(registry: &Registry<'a>, method: &str) -> Option<&'a EndpointSchemaSpec> {
    endpoint_at(registry.model, *registry.endpoints_by_method.get(method)?)
}

/// Look up a top-level definition, preserving aliases and declaration identity.
pub fn definition_by_name<'a>(registry: &Registry<'a>, name: &str) -> Option<&'a TypeDefinition> {
    registry.model.definitions.get(*registry.definitions_by_name.get(name)?)
}

/// Look up a role without applying inheritance or case conversion.
pub fn role_by_name<'a>(registry: &Registry<'a>, name: &str) -> Option<&'a RoleSpec> {
    registry.model.roles.get(*registry.roles_by_name.get(name)?)
}

/// Look up an error code in its separate namespace.
pub fn error_by_code<'a>(registry: &Registry<'a>, code: u32) -> Option<&'a ErrorSpec> {
    registry.model.errors.get(*registry.errors_by_code.get(&code)?)
}

/// Follow only nominal reference aliases, without expanding fields or containers.
///
/// References to inline declarations are also supported. Their uncommon lookup
/// path scans owned types, since the frozen registry stores only top-level
/// definition indices. The returned type always borrows the caller or model.
pub fn resolve_type<'a>(registry: &Registry<'a>, ty: &'a EndpointType) -> Result<&'a EndpointType> {
    resolve_with(ty, |name| {
        if let Some(definition) = definition_by_name(registry, name) {
            Ok(Some(&definition.ty))
        } else {
            inline_type(registry.model, name)
        }
    }).map(|(resolved, _)| resolved)
}

/// Authorize when any required role is in the transitive closure of the grants.
///
/// Every supplied grant and required role is checked before an access decision,
/// including for public endpoints and endpoints cloned from registered records.
pub fn is_authorized(
    registry: &Registry<'_>,
    endpoint: &EndpointSchemaSpec,
    granted_roles: &[&str],
) -> Result<bool> {
    let mut pending = Vec::new();
    for name in granted_roles {
        let role = role_by_name(registry, name).ok_or_else(|| invalid("unknown granted role"))?;
        pending.push(role);
    }
    required_roles(registry, &endpoint.roles)?;
    if endpoint.roles.is_empty() {
        return Ok(true);
    }
    let mut granted = BTreeSet::new();
    while let Some(role) = pending.pop() {
        if granted.insert(role.name.as_str()) {
            for name in &role.includes {
                let included = role_by_name(registry, name)
                    .ok_or_else(|| invalid("unknown included role"))?;
                pending.push(included);
            }
        }
    }
    Ok(endpoint.roles.iter().any(|name| granted.contains(name.as_str())))
}

fn invalid(detail: &'static str) -> Error {
    model_error(ErrorCode::ConstraintViolation, detail)
}

fn depth_limit() -> Error {
    model_error(ErrorCode::DepthLimitExceeded, "endpoint type traversal depth exceeded")
}

fn check_depth(depth: usize) -> Result<()> {
    if depth > MAX_REFERENCE_DEPTH { Err(depth_limit()) } else { Ok(()) }
}

fn identifier(name: &str) -> Result<()> {
    let bytes = name.as_bytes();
    let valid_start = bytes.first().is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_');
    if !valid_start || name == "_"
        || !bytes.iter().all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
    {
        return Err(invalid("invalid ASCII identifier"));
    }
    Ok(())
}

// Kept local so Wave 1 does not require the later utilities module to exist.
fn header_key(name: &str) -> Result<String> {
    if name.is_empty() || !name.bytes().all(|byte| {
        byte.is_ascii_alphanumeric()
            || matches!(byte, b'!' | b'#' | b'$' | b'%' | b'&' | b'\'' | b'*' | b'+'
                | b'-' | b'.' | b'^' | b'_' | b'`' | b'|' | b'~')
    }) {
        return Err(invalid("invalid HTTP header name"));
    }
    Ok(name.to_ascii_lowercase())
}

fn collect_fields<'a>(
    fields: &'a [EndpointField],
    depth: usize,
    types: &mut BTreeMap<&'a str, &'a EndpointType>,
) -> Result<()> {
    let mut names = BTreeSet::new();
    for field in fields {
        if field.name.is_empty() || !names.insert(field.name.as_str()) {
            return Err(invalid("empty or duplicate field name"));
        }
        collect_type(&field.ty, depth, types)?;
    }
    Ok(())
}

fn collect_type<'a>(
    ty: &'a EndpointType,
    depth: usize,
    types: &mut BTreeMap<&'a str, &'a EndpointType>,
) -> Result<()> {
    check_depth(depth)?;
    match ty {
        EndpointType::Struct { name, .. } | EndpointType::Enum { name, .. } => {
            identifier(name)?;
            if let Some(existing) = types.get(name.as_str()) {
                // A top-level nominal definition already reserved its own name.
                // Equal but separately owned declarations still collide.
                if !core::ptr::eq(*existing, ty) {
                    return Err(invalid("conflicting or repeated nominal type declaration"));
                }
            } else {
                types.insert(name.as_str(), ty);
            }
        }
        _ => {}
    }
    match ty {
        EndpointType::Struct { fields, .. } => collect_fields(fields, depth + 1, types),
        EndpointType::Enum { variants, .. } => {
            let mut names = BTreeSet::new();
            let mut values = BTreeSet::new();
            for variant in variants {
                identifier(&variant.name)?;
                if !names.insert(variant.name.as_str()) {
                    return Err(invalid("duplicate enum variant name"));
                }
                if !values.insert(variant.value) {
                    return Err(invalid("duplicate enum discriminant"));
                }
            }
            Ok(())
        }
        EndpointType::StructRef(name)
        | EndpointType::EnumRef { name, .. }
        | EndpointType::StructTable { struct_ref: name } => identifier(name),
        EndpointType::Vec(inner) | EndpointType::Optional(inner) => collect_type(inner, depth + 1, types),
        EndpointType::NanoId { len: 0 } => Err(invalid("NanoId length must be nonzero")),
        EndpointType::UInt32 | EndpointType::Int32 | EndpointType::Int64
        | EndpointType::Float64 | EndpointType::Boolean | EndpointType::String
        | EndpointType::Bytea | EndpointType::UUID | EndpointType::NanoId { .. }
        | EndpointType::IpAddr | EndpointType::Object | EndpointType::Unit
        | EndpointType::TimeStampMs | EndpointType::BlockchainDecimal
        | EndpointType::BlockchainAddress | EndpointType::BlockchainTransactionHash => Ok(()),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum NominalKind { Struct, Enum }

fn reference(ty: &EndpointType) -> Option<(&str, NominalKind)> {
    match ty {
        EndpointType::StructRef(name) | EndpointType::StructTable { struct_ref: name } => {
            Some((name, NominalKind::Struct))
        }
        EndpointType::EnumRef { name, .. } => Some((name, NominalKind::Enum)),
        _ => None,
    }
}

fn resolve_with<'a>(
    mut ty: &'a EndpointType,
    mut lookup: impl FnMut(&str) -> Result<Option<&'a EndpointType>>,
) -> Result<(&'a EndpointType, usize)> {
    let mut seen = BTreeSet::new();
    let mut expected = None;
    let mut hops = 0;
    while let Some((name, kind)) = reference(ty) {
        identifier(name)?;
        if !seen.insert(name) {
            return Err(invalid("reference alias cycle"));
        }
        if expected.is_some_and(|previous| previous != kind) {
            return Err(invalid("reference alias changes nominal kind"));
        }
        expected = Some(kind);
        if hops == MAX_REFERENCE_DEPTH {
            return Err(depth_limit());
        }
        ty = lookup(name)?.ok_or_else(|| invalid("unknown type reference"))?;
        hops += 1;
    }
    let actual = match ty {
        EndpointType::Struct { .. } => Some(NominalKind::Struct),
        EndpointType::Enum { .. } => Some(NominalKind::Enum),
        _ => None,
    };
    if expected.is_some() && expected != actual {
        return Err(invalid("reference target has the wrong nominal kind"));
    }
    Ok((ty, hops))
}

struct TypeGraph<'a, 'm> {
    types: &'m BTreeMap<&'a str, &'a EndpointType>,
    active: BTreeSet<&'a str>,
    // Revisit a declaration only when reached with less remaining depth. A
    // shallow prior visit must not conceal an over-limit deeper reference path.
    visited: BTreeMap<&'a str, usize>,
}

impl<'a> TypeGraph<'a, '_> {
    fn validate(&mut self, ty: &'a EndpointType, depth: usize) -> Result<()> {
        check_depth(depth)?;
        if reference(ty).is_some() {
            let (target, hops) = resolve_with(ty, |name| Ok(self.types.get(name).copied()))?;
            return self.validate(target, depth + hops);
        }
        match ty {
            EndpointType::Struct { name, fields } => {
                if self.active.contains(name.as_str())
                    || self.visited.get(name.as_str()).is_some_and(|previous| *previous >= depth)
                {
                    return Ok(());
                }
                self.active.insert(name.as_str());
                for field in fields {
                    self.validate(&field.ty, depth + 1)?;
                }
                self.active.remove(name.as_str());
                self.visited.insert(name.as_str(), depth);
            }
            EndpointType::Vec(inner) | EndpointType::Optional(inner) => self.validate(inner, depth + 1)?,
            _ => {}
        }
        Ok(())
    }
}

// Walk only ownership, never reference edges. This keeps inline lookup finite
// for recursive nominal graphs and avoids constructing a second registry.
fn inline_type<'a>(model: &'a EndpointModel, name: &str) -> Result<Option<&'a EndpointType>> {
    let mut pending = Vec::new();
    for service in model.services.iter().rev() {
        for endpoint in service.endpoints.iter().rev() {
            if let Some(ty) = &endpoint.stream_response {
                pending.push((ty, 0));
            }
            for field in endpoint.response.iter().rev().chain(endpoint.params.iter().rev()) {
                pending.push((&field.ty, 0));
            }
        }
    }
    for definition in &model.definitions {
        pending.push((&definition.ty, 0));
    }
    for header in &model.headers {
        pending.push((&header.ty, 0));
    }
    while let Some((ty, depth)) = pending.pop() {
        check_depth(depth)?;
        match ty {
            EndpointType::Struct { name: candidate, fields } => {
                if candidate == name {
                    return Ok(Some(ty));
                }
                for field in fields.iter().rev() {
                    pending.push((&field.ty, depth + 1));
                }
            }
            EndpointType::Enum { name: candidate, .. } if candidate == name => return Ok(Some(ty)),
            EndpointType::Vec(inner) | EndpointType::Optional(inner) => pending.push((inner.as_ref(), depth + 1)),
            _ => {}
        }
    }
    Ok(None)
}

fn required_roles(registry: &Registry<'_>, names: &[String]) -> Result<()> {
    let mut seen = BTreeSet::new();
    for name in names {
        if role_by_name(registry, name).is_none() {
            return Err(invalid("unknown required role"));
        }
        if !seen.insert(name.as_str()) {
            return Err(invalid("duplicate endpoint role"));
        }
    }
    Ok(())
}

fn validate_roles(registry: &Registry<'_>) -> Result<()> {
    for role in &registry.model.roles {
        let mut includes = BTreeSet::new();
        for name in &role.includes {
            if role_by_name(registry, name).is_none() {
                return Err(invalid("unknown included role"));
            }
            if !includes.insert(name.as_str()) {
                return Err(invalid("duplicate included role"));
            }
        }
    }
    // Iterative three-color DFS supports long role chains without using the
    // machine stack. Colors: 0 unseen, 1 on the current path, 2 completed.
    let mut colors = alloc::vec![0_u8; registry.model.roles.len()];
    let mut pending = Vec::new();
    for root in 0..registry.model.roles.len() {
        if colors.get(root).copied() == Some(2) {
            continue;
        }
        pending.push((root, false));
        while let Some((index, leaving)) = pending.pop() {
            let color = colors.get_mut(index).ok_or_else(|| invalid("invalid role index"))?;
            if leaving {
                *color = 2;
                continue;
            }
            match *color {
                1 => return Err(invalid("role inclusion cycle")),
                2 => continue,
                _ => *color = 1,
            }
            pending.push((index, true));
            let role = registry.model.roles.get(index).ok_or_else(|| invalid("invalid role index"))?;
            for name in role.includes.iter().rev() {
                let target = registry.roles_by_name.get(name.as_str())
                    .copied().ok_or_else(|| invalid("unknown included role"))?;
                pending.push((target, false));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{boxed::Box, format, vec};

    fn field(name: &str, ty: EndpointType) -> EndpointField {
        EndpointField { name: String::from(name), ty, description: String::new() }
    }

    fn record(name: &str, fields: Vec<EndpointField>) -> EndpointType {
        EndpointType::Struct { name: String::from(name), fields }
    }

    fn enumeration(name: &str) -> EndpointType {
        EndpointType::Enum {
            name: String::from(name),
            variants: vec![
                EnumVariantSpec { name: String::from("Low"), value: i32::MIN, description: String::new() },
                EnumVariantSpec { name: String::from("High"), value: i32::MAX, description: String::new() },
            ],
        }
    }

    fn definition(name: &str, ty: EndpointType) -> TypeDefinition {
        TypeDefinition { name: String::from(name), ty }
    }

    fn role(name: &str, includes: &[&str]) -> RoleSpec {
        RoleSpec {
            name: String::from(name),
            includes: includes.iter().map(|name| String::from(*name)).collect(),
            description: String::new(),
        }
    }

    fn endpoint(code: u32, service: &str, name: &str) -> EndpointSchemaSpec {
        EndpointSchemaSpec {
            code, name: String::from(name), service: String::from(service),
            roles: Vec::new(), params: Vec::new(), response: Vec::new(),
            stream_response: None, description: String::new(),
        }
    }

    fn header(name: &str, ty: EndpointType) -> HeaderSpec {
        HeaderSpec { name: String::from(name), ty, required: false, description: String::new() }
    }

    fn model() -> EndpointModel {
        let mut read = endpoint(0, "Store", "read");
        read.roles.push(String::from("Reader"));
        read.params.push(field("検索 語", EndpointType::String));
        read.response.push(field("state", EndpointType::EnumRef {
            name: String::from("StateAlias"), prefixed_name: true,
        }));
        read.stream_response = Some(EndpointType::StructTable { struct_ref: String::from("NodeAlias") });
        EndpointModel {
            version: ModelVersion::new(1, 2, 3),
            services: vec![
                ServiceSpec { id: 0, name: String::from("Store"), endpoints: vec![read] },
                ServiceSpec {
                    id: u16::MAX, name: String::from("Health"),
                    endpoints: vec![endpoint(u32::MAX, "Health", "ping")],
                },
            ],
            definitions: vec![
                definition("Node", record("Node", vec![field("children", EndpointType::Vec(Box::new(
                    EndpointType::Optional(Box::new(EndpointType::StructRef(String::from("Node")))),
                )))])),
                definition("State", enumeration("State")),
                definition("NodeAlias", EndpointType::StructRef(String::from("Node"))),
                definition("StateAlias", EndpointType::EnumRef { name: String::from("State"), prefixed_name: false }),
            ],
            roles: vec![role("Reader", &[]), role("Writer", &["Reader"]), role("Admin", &["Writer"])],
            errors: vec![
                ErrorSpec { code: 0, name: String::from("Unavailable"), message: String::new() },
                ErrorSpec { code: u32::MAX, name: String::from("Denied"), message: String::new() },
            ],
            headers: vec![header("X-Trace", EndpointType::NanoId { len: 21 })],
        }
    }

    fn failure(model: &EndpointModel, code: ErrorCode) -> Error {
        let error = build_registry(model).unwrap_err();
        assert_eq!(error.code, code);
        assert_eq!(error.source, ErrorSource::SchemaCompile);
        assert_eq!((error.byte_offset, error.schema_id, error.field_id), (0, 0, 0));
        assert!(!error.detail.is_empty());
        error
    }

    fn constraint(model: &EndpointModel) {
        failure(model, ErrorCode::ConstraintViolation);
    }

    #[test]
    fn empty_model_builds_and_all_lookups_miss() {
        let model = EndpointModel::default();
        let registry = build_registry(&model).unwrap();
        assert!(core::ptr::eq(registry.model(), &model));
        assert!(service_by_id(&registry, 0).is_none());
        assert!(service_by_name(&registry, "").is_none());
        assert!(endpoint_by_code(&registry, 0).is_none());
        assert!(endpoint_by_method(&registry, "A.b").is_none());
        assert!(definition_by_name(&registry, "Missing").is_none());
        assert!(role_by_name(&registry, "Missing").is_none());
        assert!(error_by_code(&registry, 0).is_none());
        assert!(is_authorized(&registry, &endpoint(0, "Any", "public"), &[]).unwrap());
    }

    #[test]
    fn indexes_borrow_exact_records_and_preserve_boundary_ids_and_order() {
        let model = model();
        let original = model.clone();
        let registry = build_registry(&model).unwrap();
        assert!(core::ptr::eq(service_by_id(&registry, 0).unwrap(), &model.services[0]));
        assert!(core::ptr::eq(service_by_name(&registry, "Health").unwrap(), &model.services[1]));
        assert_eq!(service_by_id(&registry, u16::MAX).unwrap().name, "Health");
        assert!(core::ptr::eq(endpoint_by_code(&registry, 0).unwrap(), &model.services[0].endpoints[0]));
        assert!(core::ptr::eq(endpoint_by_method(&registry, "Store.read").unwrap(), &model.services[0].endpoints[0]));
        assert_eq!(endpoint_by_code(&registry, u32::MAX).unwrap().name, "ping");
        assert!(core::ptr::eq(definition_by_name(&registry, "NodeAlias").unwrap(), &model.definitions[2]));
        assert!(core::ptr::eq(role_by_name(&registry, "Admin").unwrap(), &model.roles[2]));
        assert!(core::ptr::eq(error_by_code(&registry, 0).unwrap(), &model.errors[0]));
        assert_eq!(error_by_code(&registry, u32::MAX).unwrap().name, "Denied");
        assert_eq!(model, original);
        assert!(service_by_id(&registry, 1).is_none());
        assert!(service_by_name(&registry, "store").is_none());
        assert!(endpoint_by_method(&registry, "store.read").is_none());
        assert!(endpoint_by_method(&registry, "Store.Read").is_none());
        assert!(endpoint_by_method(&registry, "Store.read ").is_none());
        assert!(endpoint_by_method(&registry, "read").is_none());
        assert!(endpoint_by_code(&registry, 1).is_none());
        assert!(definition_by_name(&registry, "node").is_none());
        assert!(role_by_name(&registry, "admin").is_none());
        assert!(error_by_code(&registry, 1).is_none());
    }

    #[test]
    fn returned_records_outlive_the_index() {
        let model = model();
        let borrowed = {
            let registry = build_registry(&model).unwrap();
            endpoint_by_code(&registry, 0).unwrap()
        };
        assert!(core::ptr::eq(borrowed, &model.services[0].endpoints[0]));
    }

    #[test]
    fn lookups_use_checked_indices_even_if_internal_indexes_are_corrupted() {
        let model = model();
        let mut registry = build_registry(&model).unwrap();
        registry.services_by_id.insert(0, usize::MAX);
        registry.services_by_name.insert("Store", usize::MAX);
        registry.definitions_by_name.insert("Node", usize::MAX);
        registry.roles_by_name.insert("Reader", usize::MAX);
        registry.errors_by_code.insert(0, usize::MAX);
        registry.endpoints_by_code.insert(0, EndpointLocation { service_index: usize::MAX, endpoint_index: 0 });
        registry.endpoints_by_method.insert(String::from("Store.read"), EndpointLocation { service_index: 0, endpoint_index: usize::MAX });
        assert!(service_by_id(&registry, 0).is_none());
        assert!(service_by_name(&registry, "Store").is_none());
        assert!(definition_by_name(&registry, "Node").is_none());
        assert!(role_by_name(&registry, "Reader").is_none());
        assert!(error_by_code(&registry, 0).is_none());
        assert!(endpoint_by_code(&registry, 0).is_none());
        assert!(endpoint_by_method(&registry, "Store.read").is_none());
    }

    #[test]
    fn duplicate_top_level_keys_and_endpoint_identity_are_rejected() {
        let mut cases = Vec::new();
        let mut item = model();
        item.services[1].id = item.services[0].id;
        cases.push(item);
        let mut item = model();
        item.services[1].name = item.services[0].name.clone();
        cases.push(item);
        let mut item = model();
        item.services[1].endpoints[0].code = 0;
        cases.push(item);
        let mut item = model();
        let mut duplicate = item.services[0].endpoints[0].clone();
        duplicate.code = 7;
        item.services[0].endpoints.push(duplicate);
        cases.push(item);
        let mut item = model();
        item.services[0].endpoints[0].service = String::from("store");
        cases.push(item);
        let mut item = model();
        item.definitions.push(item.definitions[0].clone());
        cases.push(item);
        let mut item = model();
        item.roles.push(item.roles[0].clone());
        cases.push(item);
        let mut item = model();
        item.errors[1].name = item.errors[0].name.clone();
        cases.push(item);
        let mut item = model();
        item.errors[1].code = item.errors[0].code;
        cases.push(item);
        for item in &cases {
            constraint(item);
        }
    }

    #[test]
    fn endpoint_names_are_local_to_services_and_names_are_case_sensitive() {
        let mut model = model();
        model.services[1].endpoints[0].name = String::from("read");
        model.roles.push(role("reader", &[]));
        model.definitions.push(definition("node", record("node", Vec::new())));
        model.services.push(ServiceSpec { id: 42, name: String::from("store"), endpoints: Vec::new() });
        let registry = build_registry(&model).unwrap();
        assert_eq!(endpoint_by_method(&registry, "Health.read").unwrap().code, u32::MAX);
        assert_eq!(endpoint_by_method(&registry, "Store.read").unwrap().code, 0);
        assert_ne!(role_by_name(&registry, "reader"), role_by_name(&registry, "Reader"));
        assert_eq!(service_by_name(&registry, "store").unwrap().id, 42);
    }

    #[test]
    fn identifier_grammar_applies_to_every_identifier_category() {
        for name in ["", "_", "9bad", "has.dot", "has-dash", "has space", "é", "a\n", "a\0b"] {
            assert!(identifier(name).is_err(), "{name:?}");
            let mut item = model();
            item.services[0].name = String::from(name);
            constraint(&item);
            let mut item = model();
            item.services[0].endpoints[0].name = String::from(name);
            constraint(&item);
            let mut item = model();
            item.definitions[0].name = String::from(name);
            constraint(&item);
            let mut item = model();
            item.roles[0].name = String::from(name);
            constraint(&item);
            let mut item = model();
            item.errors[0].name = String::from(name);
            constraint(&item);
            let mut item = model();
            item.definitions[1].ty = enumeration(name);
            constraint(&item);
            let mut item = model();
            if let EndpointType::Enum { variants, .. } = &mut item.definitions[1].ty {
                variants[0].name = String::from(name);
            }
            constraint(&item);
            let mut item = model();
            item.headers[0].ty = record(name, Vec::new());
            constraint(&item);
        }
        for name in ["a", "A", "_a", "__", "A0_", "HTTPServerID"] {
            identifier(name).unwrap();
        }
    }

    #[test]
    fn fields_allow_utf8_and_punctuation_but_reject_empty_and_duplicate_names() {
        let mut model = model();
        model.services[0].endpoints[0].params = vec![
            field("a.b", EndpointType::String), field("a-b", EndpointType::String),
            field("é", EndpointType::String), field("e\u{301}", EndpointType::String),
            field(" ", EndpointType::String), field("\0", EndpointType::String),
        ];
        // The same field name may occur in a distinct response record.
        model.services[0].endpoints[0].response = vec![field("a.b", EndpointType::Boolean)];
        build_registry(&model).unwrap();
        model.services[0].endpoints[0].params.push(field("a.b", EndpointType::Int64));
        constraint(&model);
        model.services[0].endpoints[0].params.pop();
        model.services[0].endpoints[0].response[0].name.clear();
        constraint(&model);
        let mut model = EndpointModel::default();
        model.definitions.push(definition("Nested", record("Nested", vec![
            field("same", EndpointType::String), field("same", EndpointType::Int32),
        ])));
        constraint(&model);
    }

    #[test]
    fn registry_does_not_narrow_record_field_counts_to_schema_widths() {
        let count = usize::from(u16::MAX) + 1;
        let fields = (0..count)
            .map(|index| field(&format!("field_{index}"), EndpointType::Boolean))
            .collect();
        let model = EndpointModel {
            definitions: vec![definition("Wide", record("Wide", fields))],
            ..EndpointModel::default()
        };
        let registry = build_registry(&model).unwrap();
        match &definition_by_name(&registry, "Wide").unwrap().ty {
            EndpointType::Struct { fields, .. } => assert_eq!(fields.len(), count),
            other => panic!("expected the original record, got {other:?}"),
        }
    }

    #[test]
    fn header_tokens_are_ascii_and_case_insensitively_unique() {
        let mut model = model();
        model.headers.push(header("!#$%&'*+-.^_`|~09AZaz", EndpointType::String));
        build_registry(&model).unwrap();
        assert_eq!(header_key("X-Trace").unwrap(), "x-trace");
        model.headers.push(header("x-tRaCe", EndpointType::String));
        constraint(&model);
        for name in ["", "x y", "x:y", "x/y", "x\ty", "x\ry", "x\ny", "x\0y", "é", "x\u{7f}"] {
            let mut item = EndpointModel::default();
            item.headers.push(header(name, EndpointType::String));
            constraint(&item);
        }
    }

    #[test]
    fn nominal_declarations_reserve_one_shared_namespace() {
        let mut item = model();
        item.definitions[0].ty = record("Other", Vec::new());
        constraint(&item);
        let mut item = model();
        item.headers[0].ty = record("Node", Vec::new());
        constraint(&item);
        let mut item = model();
        item.headers[0].ty = enumeration("NodeAlias");
        constraint(&item);
        let mut item = EndpointModel::default();
        item.headers = vec![header("A", enumeration("Inline")), header("B", enumeration("Inline"))];
        constraint(&item);
        let mut item = EndpointModel::default();
        item.definitions = vec![definition("Alias", EndpointType::Optional(Box::new(record("Alias", Vec::new()))))];
        constraint(&item);
    }

    #[test]
    fn inline_nominal_forward_references_and_recursion_resolve_without_synthetic_definitions() {
        let mut model = EndpointModel::default();
        let mut read = endpoint(1, "InlineService", "read");
        read.params = vec![field("first", EndpointType::StructRef(String::from("InlineRecord")))];
        read.response = vec![field("value", record("InlineRecord", vec![
            field("self", EndpointType::Optional(Box::new(EndpointType::StructRef(String::from("InlineRecord"))))),
            field("state", EndpointType::EnumRef { name: String::from("InlineState"), prefixed_name: false }),
        ]))];
        read.stream_response = Some(enumeration("InlineState"));
        model.services.push(ServiceSpec { id: 1, name: String::from("InlineService"), endpoints: vec![read] });
        let registry = build_registry(&model).unwrap();
        let source = &model.services[0].endpoints[0];
        assert!(core::ptr::eq(resolve_type(&registry, &source.params[0].ty).unwrap(), &source.response[0].ty));
        let state_ref = EndpointType::EnumRef { name: String::from("InlineState"), prefixed_name: true };
        assert!(core::ptr::eq(resolve_type(&registry, &state_ref).unwrap(), source.stream_response.as_ref().unwrap()));
        assert!(definition_by_name(&registry, "InlineRecord").is_none());
        assert!(definition_by_name(&registry, "InlineState").is_none());
    }

    #[test]
    fn inline_nominals_in_wrappers_definitions_and_headers_can_be_referenced() {
        let mut model = EndpointModel::default();
        model.definitions = vec![
            definition("Wrapped", EndpointType::Vec(Box::new(record("Nested", Vec::new())))),
            definition("Alias", EndpointType::StructRef(String::from("Nested"))),
        ];
        model.headers = vec![header("X-State", EndpointType::Optional(Box::new(enumeration("HeaderState"))))];
        let registry = build_registry(&model).unwrap();
        let nested = EndpointType::StructTable { struct_ref: String::from("Alias") };
        assert!(matches!(resolve_type(&registry, &nested).unwrap(), EndpointType::Struct { name, .. } if name == "Nested"));
        let state = EndpointType::EnumRef { name: String::from("HeaderState"), prefixed_name: false };
        assert!(matches!(resolve_type(&registry, &state).unwrap(), EndpointType::Enum { name, .. } if name == "HeaderState"));
    }

    #[test]
    fn enum_names_and_discriminants_are_unique_with_full_i32_range() {
        let model = model();
        build_registry(&model).unwrap();
        let mut item = model.clone();
        if let EndpointType::Enum { variants, .. } = &mut item.definitions[1].ty {
            variants[1].name = variants[0].name.clone();
        }
        constraint(&item);
        let mut item = model;
        if let EndpointType::Enum { variants, .. } = &mut item.definitions[1].ty {
            variants[1].value = variants[0].value;
        }
        constraint(&item);
        let mut empty = EndpointModel::default();
        empty.definitions.push(definition("Empty", EndpointType::Enum { name: String::from("Empty"), variants: Vec::new() }));
        build_registry(&empty).unwrap();
    }

    #[test]
    fn scalar_variants_empty_records_and_nonzero_nanoid_lengths_are_valid() {
        let types = vec![
            EndpointType::UInt32, EndpointType::Int32, EndpointType::Int64, EndpointType::Float64,
            EndpointType::Boolean, EndpointType::String, EndpointType::Bytea, EndpointType::UUID,
            EndpointType::NanoId { len: 1 }, EndpointType::NanoId { len: u8::MAX }, EndpointType::IpAddr,
            EndpointType::Object, EndpointType::Unit, EndpointType::TimeStampMs, EndpointType::BlockchainDecimal,
            EndpointType::BlockchainAddress, EndpointType::BlockchainTransactionHash,
            record("Empty", Vec::new()), EndpointType::Optional(Box::new(EndpointType::Unit)),
        ];
        let mut model = EndpointModel::default();
        for (index, ty) in types.into_iter().enumerate() {
            model.headers.push(header(&format!("X-{index}"), ty));
        }
        build_registry(&model).unwrap();
        model.headers.push(header("X-Bad", EndpointType::NanoId { len: 0 }));
        constraint(&model);
    }

    #[test]
    fn aliases_resolve_to_borrowed_nominals_without_unwrapping_containers() {
        let model = model();
        let registry = build_registry(&model).unwrap();
        let struct_ref = EndpointType::StructRef(String::from("NodeAlias"));
        let table = EndpointType::StructTable { struct_ref: String::from("NodeAlias") };
        let enum_ref = EndpointType::EnumRef { name: String::from("StateAlias"), prefixed_name: true };
        assert!(core::ptr::eq(resolve_type(&registry, &struct_ref).unwrap(), &model.definitions[0].ty));
        assert!(core::ptr::eq(resolve_type(&registry, &table).unwrap(), &model.definitions[0].ty));
        assert!(core::ptr::eq(resolve_type(&registry, &enum_ref).unwrap(), &model.definitions[1].ty));
        for ty in [EndpointType::Int64, EndpointType::Optional(Box::new(struct_ref)), EndpointType::Vec(Box::new(table))] {
            assert!(core::ptr::eq(resolve_type(&registry, &ty).unwrap(), &ty));
        }
    }

    #[test]
    fn unknown_and_wrong_kind_references_fail_in_every_model_type_position() {
        let bad_types = vec![
            EndpointType::StructRef(String::from("Missing")),
            EndpointType::StructTable { struct_ref: String::from("Missing") },
            EndpointType::EnumRef { name: String::from("Missing"), prefixed_name: false },
            EndpointType::StructRef(String::from("State")),
            EndpointType::StructTable { struct_ref: String::from("StateAlias") },
            EndpointType::EnumRef { name: String::from("NodeAlias"), prefixed_name: true },
            EndpointType::StructRef(String::from("_")),
        ];
        for ty in bad_types {
            for position in 0..5 {
                let mut item = model();
                match position {
                    0 => item.services[0].endpoints[0].params.push(field("bad", ty.clone())),
                    1 => item.services[0].endpoints[0].response.push(field("bad", ty.clone())),
                    2 => item.services[0].endpoints[0].stream_response = Some(EndpointType::Vec(Box::new(
                        EndpointType::Optional(Box::new(ty.clone())),
                    ))),
                    3 => item.definitions.push(definition("Unused", record("Unused", vec![field("bad", ty.clone())]))),
                    _ => item.headers.push(header("X-Bad", ty.clone())),
                }
                constraint(&item);
            }
        }
        let model = model();
        let registry = build_registry(&model).unwrap();
        for ty in [
            EndpointType::StructRef(String::from("Missing")),
            EndpointType::StructTable { struct_ref: String::from("State") },
            EndpointType::EnumRef { name: String::from("Node"), prefixed_name: false },
        ] {
            let error = resolve_type(&registry, &ty).unwrap_err();
            assert_eq!(error.code, ErrorCode::ConstraintViolation);
            assert_eq!(error.source, ErrorSource::SchemaCompile);
        }
    }

    #[test]
    fn aliases_cannot_target_scalars_containers_or_change_nominal_kind() {
        for target in [EndpointType::Int32, EndpointType::Optional(Box::new(record("Nested", Vec::new())))] {
            let mut item = EndpointModel::default();
            item.definitions = vec![definition("Target", target), definition("Alias", EndpointType::StructRef(String::from("Target")))];
            constraint(&item);
        }
        let mut item = model();
        item.definitions[2].ty = EndpointType::EnumRef { name: String::from("State"), prefixed_name: false };
        constraint(&item);
    }

    #[test]
    fn self_and_mutual_alias_cycles_are_constraint_errors() {
        for definitions in [
            vec![definition("A", EndpointType::StructRef(String::from("A")))],
            vec![definition("A", EndpointType::StructRef(String::from("B"))), definition("B", EndpointType::StructRef(String::from("A")))],
            vec![
                definition("A", EndpointType::EnumRef { name: String::from("B"), prefixed_name: false }),
                definition("B", EndpointType::EnumRef { name: String::from("A"), prefixed_name: true }),
            ],
            vec![definition("A", EndpointType::StructTable { struct_ref: String::from("A") })],
        ] {
            let model = EndpointModel { definitions, ..EndpointModel::default() };
            constraint(&model);
        }
        let model = model();
        let mut registry = build_registry(&model).unwrap();
        // Exercise public resolution independently of construction using an
        // intentionally inconsistent private index, without changing the model.
        registry.definitions_by_name.insert("Node", 2);
        let ty = EndpointType::StructRef(String::from("Node"));
        assert_eq!(resolve_type(&registry, &ty).unwrap_err().detail, "reference alias cycle");
    }

    #[test]
    fn mutual_nominal_recursion_is_valid_but_does_not_hide_bad_sibling_fields() {
        let mut model = EndpointModel::default();
        model.definitions = vec![
            definition("A", record("A", vec![field("b", EndpointType::StructRef(String::from("B")))])),
            definition("B", record("B", vec![field("a", EndpointType::StructRef(String::from("A")))])),
        ];
        build_registry(&model).unwrap();
        if let EndpointType::Struct { fields, .. } = &mut model.definitions[1].ty {
            fields.push(field("bad", EndpointType::StructRef(String::from("Unknown"))));
        }
        constraint(&model);
    }

    fn wrapped(depth: usize) -> EndpointType {
        let mut ty = EndpointType::String;
        for index in 0..depth {
            ty = if index % 2 == 0 { EndpointType::Vec(Box::new(ty)) } else { EndpointType::Optional(Box::new(ty)) };
        }
        ty
    }

    #[test]
    fn ownership_depth_accepts_the_exact_limit_and_rejects_one_more_edge() {
        let mut model = EndpointModel::default();
        model.headers.push(header("X-Deep", wrapped(MAX_REFERENCE_DEPTH)));
        build_registry(&model).unwrap();
        model.headers[0].ty = wrapped(MAX_REFERENCE_DEPTH + 1);
        failure(&model, ErrorCode::DepthLimitExceeded);
        model.headers[0].ty = record("Outer", vec![field("deep", wrapped(MAX_REFERENCE_DEPTH))]);
        failure(&model, ErrorCode::DepthLimitExceeded);
    }

    fn alias_chain(hops: usize) -> EndpointModel {
        let mut definitions = Vec::new();
        for index in 0..hops {
            let target = if index + 1 == hops { String::from("Target") } else { format!("Alias{}", index + 1) };
            definitions.push(definition(&format!("Alias{index}"), EndpointType::StructRef(target)));
        }
        definitions.push(definition("Target", record("Target", Vec::new())));
        EndpointModel { definitions, ..EndpointModel::default() }
    }

    #[test]
    fn alias_dereference_depth_is_bounded_in_construction_and_public_resolution() {
        let model = alias_chain(MAX_REFERENCE_DEPTH);
        let registry = build_registry(&model).unwrap();
        assert!(matches!(resolve_type(&registry, &model.definitions[0].ty).unwrap(), EndpointType::Struct { name, .. } if name == "Target"));
        let extra = EndpointType::StructRef(String::from("Alias0"));
        assert_eq!(resolve_type(&registry, &extra).unwrap_err().code, ErrorCode::DepthLimitExceeded);
        failure(&alias_chain(MAX_REFERENCE_DEPTH + 1), ErrorCode::DepthLimitExceeded);
    }

    #[test]
    fn ownership_and_alias_edges_share_the_traversal_budget() {
        let mut model = alias_chain(MAX_REFERENCE_DEPTH);
        model.headers.push(header("X-Deep", EndpointType::Optional(Box::new(EndpointType::StructRef(String::from("Alias0"))))));
        failure(&model, ErrorCode::DepthLimitExceeded);
    }

    #[test]
    fn completed_shallow_graph_visits_do_not_hide_deep_reference_paths() {
        let mut model = EndpointModel::default();
        // The leaf is validated first, then reached again through a longer path.
        model.definitions.push(definition("Leaf", record("Leaf", vec![field("value", wrapped(MAX_REFERENCE_DEPTH - 2))])));
        model.definitions.push(definition("Outer", record("Outer", vec![field("leaf", EndpointType::StructRef(String::from("Leaf")))])));
        failure(&model, ErrorCode::DepthLimitExceeded);
    }

    #[test]
    fn nominal_graph_depth_is_bounded_and_alias_resolution_does_not_expand_fields() {
        let mut model = EndpointModel::default();
        for index in 0..=(MAX_REFERENCE_DEPTH / 2) {
            let fields = if index == MAX_REFERENCE_DEPTH / 2 {
                Vec::new()
            } else {
                vec![field("next", EndpointType::StructRef(format!("N{}", index + 1)))]
            };
            let name = format!("N{index}");
            model.definitions.push(definition(&name, record(&name, fields)));
        }
        let registry = build_registry(&model).unwrap();
        let ty = EndpointType::StructRef(String::from("N0"));
        assert!(core::ptr::eq(resolve_type(&registry, &ty).unwrap(), &model.definitions[0].ty));
        let last = model.definitions.len() - 1;
        if let EndpointType::Struct { fields, .. } = &mut model.definitions[last].ty {
            fields.push(field("too_far", EndpointType::String));
        }
        failure(&model, ErrorCode::DepthLimitExceeded);
    }

    #[test]
    fn role_graph_rejects_unknown_duplicate_and_cyclic_includes() {
        for roles in [
            vec![role("A", &["Missing"])],
            vec![role("A", &["B", "B"]), role("B", &[])],
            vec![role("A", &["A"])],
            vec![role("A", &["B"]), role("B", &["A"])],
            vec![role("A", &["B"]), role("B", &["C"]), role("C", &["A"])],
        ] {
            let model = EndpointModel { roles, ..EndpointModel::default() };
            constraint(&model);
        }
        let mut item = model();
        item.services[0].endpoints[0].roles.push(String::from("Reader"));
        constraint(&item);
        item.services[0].endpoints[0].roles = vec![String::from("reader")];
        constraint(&item);
    }

    #[test]
    fn authorization_is_transitive_directional_and_any_required_role_suffices() {
        let model = model();
        let registry = build_registry(&model).unwrap();
        let read = &model.services[0].endpoints[0];
        assert!(!is_authorized(&registry, read, &[]).unwrap());
        for grants in [&["Reader"][..], &["Writer"][..], &["Admin"][..], &["Admin", "Admin"][..]] {
            assert!(is_authorized(&registry, read, grants).unwrap());
        }
        let mut clone = read.clone();
        clone.roles = vec![String::from("Admin")];
        assert!(!is_authorized(&registry, &clone, &["Reader"]).unwrap());
        assert!(!is_authorized(&registry, &clone, &["Writer"]).unwrap());
        clone.roles.push(String::from("Reader"));
        assert!(is_authorized(&registry, &clone, &["Reader"]).unwrap());
        assert!(is_authorized(&registry, &model.services[1].endpoints[0], &[]).unwrap());
    }

    #[test]
    fn unknown_grants_or_required_roles_never_implicitly_authorize() {
        let model = model();
        let registry = build_registry(&model).unwrap();
        let read = &model.services[0].endpoints[0];
        let public = &model.services[1].endpoints[0];
        for endpoint in [read, public] {
            for grants in [&["Missing"][..], &["Admin", "Missing"][..], &["reader"][..], &[""][..]] {
                let error = is_authorized(&registry, endpoint, grants).unwrap_err();
                assert_eq!(error.code, ErrorCode::ConstraintViolation);
                assert_eq!(error.source, ErrorSource::SchemaCompile);
            }
        }
        let mut clone = read.clone();
        clone.roles.push(String::from("Missing"));
        assert_eq!(is_authorized(&registry, &clone, &["Admin"]).unwrap_err().code, ErrorCode::ConstraintViolation);
        clone.roles = vec![String::from("Reader"), String::from("Reader")];
        assert_eq!(is_authorized(&registry, &clone, &["Admin"]).unwrap_err().code, ErrorCode::ConstraintViolation);
    }

    #[test]
    fn diamond_inheritance_and_long_role_chains_are_finite() {
        let mut model = model();
        model.roles = vec![role("Reader", &[]), role("Left", &["Reader"]), role("Right", &["Reader"]), role("Top", &["Left", "Right"])];
        let registry = build_registry(&model).unwrap();
        assert!(is_authorized(&registry, &model.services[0].endpoints[0], &["Top"]).unwrap());
        for index in 0..(MAX_REFERENCE_DEPTH * 2) {
            let included = if index == 0 { String::from("Top") } else { format!("Role{}", index - 1) };
            model.roles.push(role(&format!("Role{index}"), &[&included]));
        }
        let registry = build_registry(&model).unwrap();
        let top = format!("Role{}", MAX_REFERENCE_DEPTH * 2 - 1);
        assert!(is_authorized(&registry, &model.services[0].endpoints[0], &[&top]).unwrap());
    }

    #[test]
    fn construction_failure_order_is_deterministic() {
        let mut model = model();
        model.services[1].id = 0;
        model.headers[0].ty = EndpointType::StructRef(String::from("Missing"));
        let first = failure(&model, ErrorCode::ConstraintViolation);
        assert_eq!(first.detail, "duplicate service id");
        for _ in 0..4 {
            assert_eq!(build_registry(&model).unwrap_err(), first);
        }
    }
}
