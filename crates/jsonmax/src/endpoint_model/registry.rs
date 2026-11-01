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
/// path scans owned types, since the registry stores only top-level
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

// Keep header-name normalization here so registry validation has no dependency
// on higher-level utilities.
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
