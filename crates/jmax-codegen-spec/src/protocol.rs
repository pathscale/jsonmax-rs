//! Deterministic OpenAPI 3.1 and AsyncAPI 3.0 endpoint projections.
//!
//! Catalog validation precedes visibility filtering. Borrowed definition tables
//! and explicit traversal stacks support recursive named graphs without cloning
//! endpoint trees or recursively expanding references. All output is bounded as
//! it is written, including escaped strings and absolute document references.

use super::*;

/// Projects visible endpoints as POST operations with separate request,
/// immediate response, and optional stream schemas. The complete catalog and
/// registry are validated, including definitions belonging to hidden endpoints.
pub fn openapi(
    services: &[ServiceSpec],
    registry: &TypeRegistry,
    visibility: Visibility<'_>,
    options: &DocumentOptions,
) -> Result<GeneratedDocument> {
    let context = catalog_context(services, registry, options.max_type_depth)?;
    let server = options.server_url.as_deref().map(parse_server_url).transpose()?;
    let mut out = Writer::new(options.max_output_bytes);
    out.raw("{\"openapi\":")?;
    out.string(OPENAPI_VERSION)?;
    write_info(&mut out, options)?;
    if server.is_some() {
        out.raw(",\"servers\":[{\"url\":")?;
        // Parsing above established that this is an absolute HTTP(S) URI.
        if let Some(url) = &options.server_url {
            out.string(url)?;
        }
        out.raw("}]")?;
    }
    out.raw(",\"paths\":{")?;
    let mut first = true;
    for service in services {
        for endpoint in &service.endpoints {
            if !visibility.includes(endpoint) {
                continue;
            }
            out.separator(&mut first)?;
            let path = rpc_path(service, endpoint, options.max_output_bytes)?;
            out.string(&path)?;
            out.raw(":{\"post\":{\"operationId\":")?;
            write_tool_name(&mut out, service, endpoint)?;
            out.raw(",\"description\":")?;
            out.string(&endpoint.description)?;
            out.raw(",\"tags\":[")?;
            out.string(&service.name)?;
            out.raw("]")?;
            write_extensions(&mut out, endpoint)?;
            out.raw(",\"requestBody\":{\"required\":true,\"content\":{\"application/json\":{\"schema\":")?;
            write_schema(&mut out, &context, Root::Fields(&endpoint.params), &[
                "paths", &path, "post", "requestBody", "content", "application/json", "schema",
            ])?;
            out.raw("}}},\"responses\":{\"200\":{\"description\":\"Successful response\",\"content\":{\"application/json\":{\"schema\":")?;
            write_schema(&mut out, &context, Root::Fields(&endpoint.response), &[
                "paths", &path, "post", "responses", "200", "content", "application/json", "schema",
            ])?;
            out.raw("}}}}")?;
            if let Some(ty) = &endpoint.stream_response {
                out.raw(",\"x-jmax-stream-schema\":")?;
                write_schema(&mut out, &context, Root::Type(ty), &[
                    "paths", &path, "post", "x-jmax-stream-schema",
                ])?;
            }
            out.raw("}}")?;
        }
    }
    out.raw("}}\n")?;
    GeneratedDocument::new("application/json", out.text, options.max_output_bytes)
}

/// Projects only visible streaming endpoints as result channels and send
/// operations. Server hosts include an explicit port when supplied; base paths
/// are retained as AsyncAPI pathname values. No security scheme is inferred.
pub fn asyncapi(
    services: &[ServiceSpec],
    registry: &TypeRegistry,
    visibility: Visibility<'_>,
    options: &DocumentOptions,
) -> Result<GeneratedDocument> {
    let context = catalog_context(services, registry, options.max_type_depth)?;
    let server = options.server_url.as_deref().map(parse_server_url).transpose()?;
    let mut out = Writer::new(options.max_output_bytes);
    out.raw("{\"asyncapi\":")?;
    out.string(ASYNCAPI_VERSION)?;
    write_info(&mut out, options)?;
    if let Some(server) = server {
        out.raw(",\"servers\":{\"default\":{\"host\":")?;
        out.string(server.host)?;
        out.raw(",\"protocol\":")?;
        out.string(server.protocol)?;
        if !server.pathname.is_empty() {
            out.raw(",\"pathname\":")?;
            out.string(server.pathname)?;
        }
        out.raw("}}")?;
    }
    out.raw(",\"channels\":{")?;
    let mut first = true;
    for service in services {
        for endpoint in &service.endpoints {
            if !visibility.includes(endpoint) {
                continue;
            }
            let Some(ty) = &endpoint.stream_response else { continue };
            out.separator(&mut first)?;
            // Charge the raw length before allocating the shared tool identity.
            let name = bounded_tool_name(service, endpoint, options.max_output_bytes)?;
            out.string(&name)?;
            out.raw(":{\"address\":")?;
            out.string_parts(&["/rpc/", &service.name, "/", &endpoint.name])?;
            out.raw(",\"description\":")?;
            out.string(&endpoint.description)?;
            out.raw(",\"messages\":{\"result\":{\"name\":")?;
            out.string(&name)?;
            out.raw(",\"contentType\":\"application/json\",\"payload\":")?;
            write_schema(&mut out, &context, Root::Type(ty), &[
                "channels", &name, "messages", "result", "payload",
            ])?;
            out.raw("}}}")?;
        }
    }
    out.raw("},\"operations\":{")?;
    let mut first = true;
    for service in services {
        for endpoint in &service.endpoints {
            if !visibility.includes(endpoint) || endpoint.stream_response.is_none() {
                continue;
            }
            out.separator(&mut first)?;
            let name = bounded_tool_name(service, endpoint, options.max_output_bytes)?;
            out.string(&name)?;
            out.raw(":{\"action\":\"send\",\"channel\":{")?;
            out.reference(&["channels", &name])?;
            out.raw("},\"messages\":[{")?;
            out.reference(&["channels", &name, "messages", "result"])?;
            out.raw("}],\"description\":")?;
            out.string(&endpoint.description)?;
            out.raw(",\"tags\":[{\"name\":")?;
            out.string(&service.name)?;
            out.raw("}]")?;
            write_extensions(&mut out, endpoint)?;
            out.raw("}")?;
        }
    }
    out.raw("}}\n")?;
    GeneratedDocument::new("application/json", out.text, options.max_output_bytes)
}

fn catalog_context<'a>(
    services: &'a [ServiceSpec], registry: &'a TypeRegistry, max_depth: u16,
) -> Result<Context<'a>> {
    validate_services(services)?;
    json_schema::validate_registry(registry, max_depth)?;
    // Build a borrowed catalog-wide index as well: endpoint-local schema calls
    // cannot resolve definitions declared inline by another endpoint.
    let mut context = Context::registry(registry, max_depth)?;
    for service in services {
        for endpoint in &service.endpoints {
            context.endpoint(endpoint)?;
        }
    }
    context.resolve()?;
    Ok(context)
}

fn write_info(out: &mut Writer, options: &DocumentOptions) -> Result<()> {
    out.raw(",\"info\":{\"title\":")?;
    out.string(&options.title)?;
    out.raw(",\"version\":")?;
    out.string(&options.version)?;
    out.raw(",\"description\":")?;
    out.string(&options.description)?;
    out.raw("}")
}

fn write_tool_name(out: &mut Writer, service: &ServiceSpec, endpoint: &EndpointSchemaSpec) -> Result<()> {
    // Catalog validation already checked both identifiers. The byte sequence is
    // exactly root tool_name, without an unnecessary temporary allocation.
    out.string_parts(&[&service.name, ".", &endpoint.name])
}

fn bounded_tool_name(service: &ServiceSpec, endpoint: &EndpointSchemaSpec, limit: u32) -> Result<String> {
    let length = service.name.len().checked_add(endpoint.name.len())
        .and_then(|length| length.checked_add(1)).ok_or_else(output_error)?;
    if length > limit as usize {
        return Err(output_error());
    }
    tool_name(&service.name, &endpoint.name)
}

fn rpc_path(service: &ServiceSpec, endpoint: &EndpointSchemaSpec, limit: u32) -> Result<String> {
    let mut path = Writer::new(limit);
    path.raw("/rpc/")?;
    path.raw(&service.name)?;
    path.raw("/")?;
    path.raw(&endpoint.name)?;
    Ok(path.text)
}

fn write_extensions(out: &mut Writer, endpoint: &EndpointSchemaSpec) -> Result<()> {
    out.raw(",\"x-jmax-code\":")?;
    out.integer(i64::from(endpoint.code))?;
    out.raw(",\"x-jmax-roles\":[")?;
    for (index, role) in endpoint.roles.iter().enumerate() {
        if index != 0 { out.raw(",")?; }
        out.string(role)?;
    }
    out.raw("]")
}

fn allocation_error() -> Error {
    spec_error(ErrorCode::BudgetExhausted, "protocol allocation failed")
}

fn output_error() -> Error {
    spec_error(ErrorCode::PayloadTooLarge, "protocol output exceeds byte limit")
}

fn invalid(detail: &'static str) -> Error {
    spec_error(ErrorCode::ConstraintViolation, detail)
}

fn duplicate(detail: &'static str) -> Error {
    spec_error(ErrorCode::DuplicateObjectKey, detail)
}

fn push<T>(values: &mut Vec<T>, value: T) -> Result<()> {
    values.try_reserve(1).map_err(|_| allocation_error())?;
    values.push(value);
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Struct,
    Enum,
}

struct Definition<'a> {
    name: &'a str,
    ty: &'a EndpointType,
    registered: bool,
}

struct Context<'a> {
    definitions: Vec<Definition<'a>>,
    references: Vec<(&'a str, Kind)>,
    max_depth: u16,
}

enum Visit<'a> {
    Type(&'a EndpointType, u32, bool),
    Fields(&'a [EndpointField], u32),
}

fn named(ty: &EndpointType) -> Result<(&str, Kind)> {
    match ty {
        EndpointType::Struct { name, .. } => Ok((name, Kind::Struct)),
        EndpointType::Enum { name, .. } => Ok((name, Kind::Enum)),
        _ => Err(invalid("registry entry must be a named definition")),
    }
}

impl<'a> Context<'a> {
    fn registry(registry: &'a TypeRegistry, max_depth: u16) -> Result<Self> {
        if max_depth == 0 {
            return Err(invalid("maximum type depth must be nonzero"));
        }
        let mut context = Self {
            definitions: Vec::new(), references: Vec::new(), max_depth,
        };
        // Reserve all top-level names first, allowing forward references and
        // inline copies of definitions registered later in the input.
        for (types, expected) in [(&registry.structs, Kind::Struct), (&registry.enums, Kind::Enum)] {
            for ty in types {
                if named(ty)?.1 != expected {
                    return Err(invalid("definition is in the wrong registry list"));
                }
                context.declare(ty, true)?;
            }
        }
        for ty in registry.structs.iter().chain(&registry.enums) {
            context.visit(Visit::Type(ty, 1, true))?;
        }
        context.resolve()?;
        Ok(context)
    }

    fn declare(&mut self, ty: &'a EndpointType, registered: bool) -> Result<bool> {
        let (name, _) = named(ty)?;
        if !valid_identifier(name) {
            return Err(invalid("invalid named definition identifier"));
        }
        if let Some(previous) = self.definitions.iter().find(|entry| entry.name == name) {
            if !registered && previous.registered && equal_types(previous.ty, ty)? {
                return Ok(false);
            }
            return Err(duplicate("duplicate or conflicting named definition"));
        }
        push(&mut self.definitions, Definition { name, ty, registered })?;
        Ok(true)
    }

    fn definition(&self, name: &str, expected: Kind) -> Result<&'a EndpointType> {
        let entry = self.definitions.iter().find(|entry| entry.name == name)
            .ok_or_else(|| invalid("unresolved named type reference"))?;
        if named(entry.ty)?.1 != expected {
            return Err(invalid("named type reference has the wrong kind"));
        }
        Ok(entry.ty)
    }

    fn resolve(&self) -> Result<()> {
        for (name, kind) in &self.references {
            self.definition(name, *kind)?;
        }
        Ok(())
    }

    fn endpoint(&mut self, endpoint: &'a EndpointSchemaSpec) -> Result<()> {
        if !valid_identifier(&endpoint.service) || !valid_identifier(&endpoint.name) {
            return Err(invalid("invalid endpoint service or name"));
        }
        for (index, role) in endpoint.roles.iter().enumerate() {
            if role.is_empty() || endpoint.roles[..index].contains(role) {
                return Err(invalid("empty or duplicate endpoint role"));
            }
        }
        for fields in [&endpoint.params, &endpoint.response] {
            validate_fields(fields)?;
            self.visit(Visit::Fields(fields, 1))?;
        }
        if let Some(ty) = &endpoint.stream_response {
            self.visit(Visit::Type(ty, 1, false))?;
        }
        Ok(())
    }

    fn visit(&mut self, first: Visit<'a>) -> Result<()> {
        let mut pending = Vec::new();
        push(&mut pending, first)?;
        while let Some(task) = pending.pop() {
            match task {
                Visit::Fields(fields, depth) => {
                    if let Some((field, rest)) = fields.split_first() {
                        push(&mut pending, Visit::Fields(rest, depth))?;
                        push(&mut pending, Visit::Type(&field.ty, depth, false))?;
                    }
                }
                Visit::Type(ty, depth, registered_root) => {
                    if depth > u32::from(self.max_depth) {
                        return Err(spec_error(ErrorCode::DepthLimitExceeded, "inline type exceeds depth limit"));
                    }
                    match ty {
                        EndpointType::Struct { fields, .. } => {
                            if !registered_root && !self.declare(ty, false)? {
                                // The registered tree already supplies its nested
                                // declarations and reference edges. A copy still
                                // has to fit at this particular inline depth.
                                validate_owned_depth(ty, depth, self.max_depth)?;
                                continue;
                            }
                            validate_fields(fields)?;
                            push(&mut pending, Visit::Fields(fields, depth + 1))?;
                        }
                        EndpointType::Enum { variants, .. } => {
                            if !registered_root {
                                self.declare(ty, false)?;
                            }
                            for (index, variant) in variants.iter().enumerate() {
                                if !valid_identifier(&variant.name) {
                                    return Err(invalid("invalid enum variant identifier"));
                                }
                                if variants[..index].iter().any(|previous| {
                                    previous.name == variant.name || previous.value == variant.value
                                }) {
                                    return Err(duplicate("duplicate enum variant name or value"));
                                }
                            }
                        }
                        EndpointType::StructRef(name) | EndpointType::StructTable { struct_ref: name } => {
                            self.reference(name, Kind::Struct)?;
                        }
                        EndpointType::EnumRef { name, .. } => self.reference(name, Kind::Enum)?,
                        EndpointType::Vec(child) | EndpointType::Optional(child) => {
                            push(&mut pending, Visit::Type(child, depth + 1, false))?;
                        }
                        EndpointType::NanoId { len: 0 } => return Err(invalid("NanoId length must be nonzero")),
                        EndpointType::UInt32 | EndpointType::Int32 | EndpointType::Int64
                        | EndpointType::Float64 | EndpointType::Boolean | EndpointType::String
                        | EndpointType::Bytea | EndpointType::UUID | EndpointType::NanoId { .. }
                        | EndpointType::IpAddr | EndpointType::Object | EndpointType::Unit
                        | EndpointType::TimeStampMs | EndpointType::BlockchainDecimal
                        | EndpointType::BlockchainAddress | EndpointType::BlockchainTransactionHash => {}
                    }
                }
            }
        }
        Ok(())
    }

    fn reference(&mut self, name: &'a str, kind: Kind) -> Result<()> {
        if !valid_identifier(name) {
            return Err(invalid("invalid named reference identifier"));
        }
        // Repeated edges do not need additional validation storage.
        if !self.references.contains(&(name, kind)) {
            push(&mut self.references, (name, kind))?;
        }
        Ok(())
    }
}

fn validate_fields(fields: &[EndpointField]) -> Result<()> {
    for (index, field) in fields.iter().enumerate() {
        if field.name.is_empty() || fields[..index].iter().any(|previous| previous.name == field.name) {
            return Err(duplicate("empty or duplicate field name"));
        }
    }
    Ok(())
}

fn validate_owned_depth(ty: &EndpointType, depth: u32, maximum: u16) -> Result<()> {
    let mut pending = Vec::new();
    push(&mut pending, Visit::Type(ty, depth, false))?;
    while let Some(task) = pending.pop() {
        match task {
            Visit::Fields(fields, depth) => {
                if let Some((field, rest)) = fields.split_first() {
                    push(&mut pending, Visit::Fields(rest, depth))?;
                    push(&mut pending, Visit::Type(&field.ty, depth, false))?;
                }
            }
            Visit::Type(ty, depth, _) => {
                if depth > u32::from(maximum) {
                    return Err(spec_error(ErrorCode::DepthLimitExceeded, "inline type exceeds depth limit"));
                }
                match ty {
                    EndpointType::Struct { fields, .. } => {
                        push(&mut pending, Visit::Fields(fields, depth + 1))?;
                    }
                    EndpointType::Vec(child) | EndpointType::Optional(child) => {
                        push(&mut pending, Visit::Type(child, depth + 1, false))?;
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

enum EqualTask<'a> {
    Types(&'a EndpointType, &'a EndpointType),
    Fields(&'a [EndpointField], &'a [EndpointField]),
}

// Derived EndpointType equality recurses through owned children. Use a fallible
// explicit stack even when comparing equal deeply nested registered copies.
fn equal_types(left: &EndpointType, right: &EndpointType) -> Result<bool> {
    let mut pending = Vec::new();
    push(&mut pending, EqualTask::Types(left, right))?;
    while let Some(task) = pending.pop() {
        match task {
            EqualTask::Fields(left, right) => {
                if left.len() != right.len() {
                    return Ok(false);
                }
                if let (Some((left, left_rest)), Some((right, right_rest))) = (left.split_first(), right.split_first()) {
                    if left.name != right.name || left.description != right.description {
                        return Ok(false);
                    }
                    push(&mut pending, EqualTask::Fields(left_rest, right_rest))?;
                    push(&mut pending, EqualTask::Types(&left.ty, &right.ty))?;
                }
            }
            EqualTask::Types(left, right) => {
                if core::ptr::eq(left, right) {
                    continue;
                }
                if core::mem::discriminant(left) != core::mem::discriminant(right) {
                    return Ok(false);
                }
                match (left, right) {
                    (EndpointType::Struct { name: a, fields: af }, EndpointType::Struct { name: b, fields: bf }) => {
                        if a != b { return Ok(false); }
                        push(&mut pending, EqualTask::Fields(af, bf))?;
                    }
                    (EndpointType::Enum { name: a, variants: av }, EndpointType::Enum { name: b, variants: bv }) => {
                        if a != b || av != bv { return Ok(false); }
                    }
                    (EndpointType::Vec(a), EndpointType::Vec(b))
                    | (EndpointType::Optional(a), EndpointType::Optional(b)) => {
                        push(&mut pending, EqualTask::Types(a, b))?;
                    }
                    (EndpointType::StructRef(a), EndpointType::StructRef(b))
                    | (EndpointType::StructTable { struct_ref: a }, EndpointType::StructTable { struct_ref: b }) => {
                        if a != b { return Ok(false); }
                    }
                    (EndpointType::EnumRef { name: a, prefixed_name: ap }, EndpointType::EnumRef { name: b, prefixed_name: bp }) => {
                        if a != b || ap != bp { return Ok(false); }
                    }
                    (EndpointType::NanoId { len: a }, EndpointType::NanoId { len: b }) => {
                        if a != b { return Ok(false); }
                    }
                    _ => {} // Equal discriminants of the remaining unit variants.
                }
            }
        }
    }
    Ok(true)
}

struct Writer {
    text: String,
    limit: usize,
}

impl Writer {
    fn new(limit: u32) -> Self {
        Self { text: String::new(), limit: limit as usize }
    }

    fn raw(&mut self, text: &str) -> Result<()> {
        if text.len() > self.limit.saturating_sub(self.text.len()) {
            return Err(output_error());
        }
        self.text.try_reserve(text.len()).map_err(|_| allocation_error())?;
        self.text.push_str(text);
        Ok(())
    }

    fn string(&mut self, text: &str) -> Result<()> {
        self.string_parts(&[text])
    }

    fn string_parts(&mut self, parts: &[&str]) -> Result<()> {
        self.raw("\"")?;
        for part in parts {
            self.escaped(part)?;
        }
        self.raw("\"")
    }

    fn escaped(&mut self, text: &str) -> Result<()> {
        let mut start = 0;
        for (index, byte) in text.bytes().enumerate() {
            let escape = match byte {
                b'"' => "\\\"",
                b'\\' => "\\\\",
                b'\x08' => "\\b",
                b'\x0c' => "\\f",
                b'\n' => "\\n",
                b'\r' => "\\r",
                b'\t' => "\\t",
                0..=0x1f => "",
                _ => continue,
            };
            self.raw(&text[start..index])?;
            if escape.is_empty() {
                const HEX: &[u8; 16] = b"0123456789abcdef";
                let bytes = [b'\\', b'u', b'0', b'0', HEX[(byte >> 4) as usize], HEX[(byte & 15) as usize]];
                self.raw(core::str::from_utf8(&bytes).map_err(|_| {
                    spec_error(ErrorCode::InternalError, "invalid ASCII escape")
                })?)?;
            } else {
                self.raw(escape)?;
            }
            start = index + 1;
        }
        self.raw(&text[start..])
    }

    fn integer(&mut self, value: i64) -> Result<()> {
        let mut bytes = [0u8; 20];
        let mut start = bytes.len();
        let mut magnitude = value.unsigned_abs();
        loop {
            start -= 1;
            bytes[start] = b'0' + (magnitude % 10) as u8;
            magnitude /= 10;
            if magnitude == 0 { break; }
        }
        if value < 0 {
            start -= 1;
            bytes[start] = b'-';
        }
        self.raw(core::str::from_utf8(&bytes[start..]).map_err(|_| {
            spec_error(ErrorCode::InternalError, "invalid ASCII integer")
        })?)
    }

    fn separator(&mut self, first: &mut bool) -> Result<()> {
        if *first {
            *first = false;
            Ok(())
        } else {
            self.raw(",")
        }
    }

    // Pointer escaping precedes JSON escaping, including slashes in path keys.
    fn pointer_token(&mut self, token: &str) -> Result<()> {
        self.raw("/")?;
        let mut start = 0;
        for (index, byte) in token.bytes().enumerate() {
            if matches!(byte, b'~' | b'/') {
                self.escaped(&token[start..index])?;
                self.raw(if byte == b'~' { "~0" } else { "~1" })?;
                start = index + 1;
            }
        }
        self.escaped(&token[start..])
    }

    fn reference(&mut self, location: &[&str]) -> Result<()> {
        self.raw("\"$ref\":\"#")?;
        for token in location {
            self.pointer_token(token)?;
        }
        self.raw("\"")
    }
}

#[derive(Clone, Copy)]
enum Root<'a> {
    Type(&'a EndpointType),
    Fields(&'a [EndpointField]),
}

enum Task<'a> {
    Schema(&'a EndpointType, Option<&'a str>),
    Body(&'a EndpointType, bool),
    Fields(&'a [EndpointField]),
    Properties(&'a [EndpointField]),
    ObjectTail(&'a [EndpointField]),
    Description(&'a str),
    Raw(&'static str),
}

struct Renderer<'a, 'c, 'o, 'p> {
    context: &'c Context<'a>,
    definitions: Vec<&'a EndpointType>,
    out: &'o mut Writer,
    location: &'p [&'p str],
}

// Each embedded schema owns its reachable $defs. References use the schema's
// full document pointer, so even recursive graphs resolve in the protocol root.
fn write_schema<'a>(
    out: &mut Writer,
    context: &Context<'a>,
    root: Root<'a>,
    location: &[&str],
) -> Result<()> {
    let mut renderer = Renderer { context, definitions: Vec::new(), out, location };
    renderer.out.raw("{\"$schema\":")?;
    renderer.out.string(JSON_SCHEMA_DIALECT)?;
    renderer.out.raw(",")?;
    renderer.render(match root {
        Root::Type(ty) => Task::Body(ty, false),
        Root::Fields(fields) => Task::Fields(fields),
    })?;
    if !renderer.definitions.is_empty() {
        renderer.out.raw(",\"$defs\":{")?;
        let mut index = 0;
        while index < renderer.definitions.len() {
            let ty = renderer.definitions[index];
            if index != 0 { renderer.out.raw(",")?; }
            renderer.out.string(named(ty)?.0)?;
            renderer.out.raw(":{")?;
            renderer.render(Task::Body(ty, true))?;
            renderer.out.raw("}")?;
            index += 1;
        }
        renderer.out.raw("}")?;
    }
    renderer.out.raw("}")
}

impl<'a> Renderer<'a, '_, '_, '_> {
    fn reference(&mut self, name: &str, kind: Kind) -> Result<()> {
        let ty = self.context.definition(name, kind)?;
        if !self.definitions.iter().any(|previous| core::ptr::eq(*previous, ty)) {
            push(&mut self.definitions, ty)?;
        }
        self.out.raw("\"$ref\":\"#")?;
        for token in self.location {
            self.out.pointer_token(token)?;
        }
        self.out.pointer_token("$defs")?;
        self.out.pointer_token(name)?;
        self.out.raw("\"")
    }

    fn render(&mut self, first: Task<'a>) -> Result<()> {
        let mut tasks = Vec::new();
        push(&mut tasks, first)?;
        while let Some(task) = tasks.pop() {
            match task {
                Task::Raw(text) => self.out.raw(text)?,
                Task::Description(text) => {
                    self.out.raw(",\"description\":")?;
                    self.out.string(text)?;
                }
                Task::Schema(ty, description) => {
                    self.out.raw("{")?;
                    push(&mut tasks, Task::Raw("}"))?;
                    if let Some(description) = description {
                        push(&mut tasks, Task::Description(description))?;
                    }
                    push(&mut tasks, Task::Body(ty, false))?;
                }
                Task::Fields(fields) => {
                    self.out.raw("\"type\":\"object\",\"properties\":{")?;
                    push(&mut tasks, Task::ObjectTail(fields))?;
                    push(&mut tasks, Task::Properties(fields))?;
                }
                Task::Properties(fields) => {
                    if let Some((field, rest)) = fields.split_first() {
                        self.out.string(&field.name)?;
                        self.out.raw(":")?;
                        push(&mut tasks, Task::Properties(rest))?;
                        if !rest.is_empty() { push(&mut tasks, Task::Raw(","))?; }
                        push(&mut tasks, Task::Schema(&field.ty, Some(&field.description)))?;
                    }
                }
                Task::ObjectTail(fields) => {
                    self.out.raw("},\"required\":[")?;
                    let mut first = true;
                    for field in fields {
                        if !matches!(&field.ty, EndpointType::Optional(_)) {
                            if !first { self.out.raw(",")?; }
                            first = false;
                            self.out.string(&field.name)?;
                        }
                    }
                    self.out.raw("],\"additionalProperties\":false")?;
                }
                Task::Body(ty, definition) => match ty {
                    EndpointType::UInt32 => self.out.raw("\"type\":\"integer\",\"minimum\":0,\"maximum\":4294967295")?,
                    EndpointType::Int32 => self.out.raw("\"type\":\"integer\",\"minimum\":-2147483648,\"maximum\":2147483647")?,
                    EndpointType::Int64 => self.out.raw("\"type\":\"integer\",\"minimum\":-9223372036854775808,\"maximum\":9223372036854775807")?,
                    EndpointType::Float64 => self.out.raw("\"type\":\"number\"")?,
                    EndpointType::Boolean => self.out.raw("\"type\":\"boolean\"")?,
                    EndpointType::String => self.out.raw("\"type\":\"string\"")?,
                    EndpointType::Bytea => self.out.raw("\"type\":\"string\",\"contentEncoding\":\"base64\"")?,
                    EndpointType::UUID => self.out.raw("\"type\":\"string\",\"format\":\"uuid\"")?,
                    EndpointType::NanoId { len } => {
                        self.out.raw("\"type\":\"string\",\"minLength\":")?;
                        self.out.integer(i64::from(*len))?;
                        self.out.raw(",\"maxLength\":")?;
                        self.out.integer(i64::from(*len))?;
                        self.out.raw(",\"pattern\":\"^[A-Za-z0-9_-]+$\"")?;
                    }
                    EndpointType::IpAddr => self.out.raw("\"anyOf\":[{\"type\":\"string\",\"format\":\"ipv4\"},{\"type\":\"string\",\"format\":\"ipv6\"}]")?,
                    EndpointType::Struct { name, fields } => {
                        if definition {
                            push(&mut tasks, Task::Fields(fields))?;
                        } else {
                            self.reference(name, Kind::Struct)?;
                        }
                    }
                    EndpointType::StructRef(name) => self.reference(name, Kind::Struct)?,
                    EndpointType::Object => self.out.raw("\"type\":\"object\"")?,
                    EndpointType::StructTable { struct_ref } => {
                        self.out.raw("\"type\":\"object\",\"additionalProperties\":{")?;
                        self.reference(struct_ref, Kind::Struct)?;
                        self.out.raw("}")?;
                    }
                    EndpointType::Vec(child) => {
                        self.out.raw("\"type\":\"array\",\"items\":")?;
                        push(&mut tasks, Task::Schema(child, None))?;
                    }
                    EndpointType::Unit => self.out.raw("\"type\":\"null\"")?,
                    EndpointType::Optional(child) => {
                        self.out.raw("\"anyOf\":[")?;
                        push(&mut tasks, Task::Raw(",{\"type\":\"null\"}]"))?;
                        push(&mut tasks, Task::Schema(child, None))?;
                    }
                    EndpointType::Enum { name, variants } => {
                        if definition {
                            self.enum_body(name, variants)?;
                        } else {
                            self.reference(name, Kind::Enum)?;
                        }
                    }
                    EndpointType::EnumRef { name, prefixed_name } => {
                        self.reference(name, Kind::Enum)?;
                        if *prefixed_name {
                            if let EndpointType::Enum { variants, .. } = self.context.definition(name, Kind::Enum)? {
                                self.enum_names(name, variants, true)?;
                            }
                        }
                    }
                    EndpointType::TimeStampMs => self.out.raw("\"type\":\"integer\",\"minimum\":-9223372036854775808,\"maximum\":9223372036854775807,\"x-unit\":\"milliseconds\"")?,
                    EndpointType::BlockchainDecimal => {
                        self.out.raw("\"type\":\"string\",\"pattern\":")?;
                        self.out.string(r"^-?[0-9]+(\.[0-9]+)?$")?;
                    }
                    EndpointType::BlockchainAddress | EndpointType::BlockchainTransactionHash => {
                        self.out.raw("\"type\":\"string\",\"minLength\":1,\"x-jmax-type\":")?;
                        self.out.string(if matches!(ty, EndpointType::BlockchainAddress) {
                            "BlockchainAddress"
                        } else {
                            "BlockchainTransactionHash"
                        })?;
                    }
                },
            }
        }
        Ok(())
    }

    fn enum_body(&mut self, name: &str, variants: &[EnumVariantSpec]) -> Result<()> {
        self.out.raw("\"type\":\"integer\",\"enum\":[")?;
        for (index, variant) in variants.iter().enumerate() {
            if index != 0 { self.out.raw(",")?; }
            self.out.integer(i64::from(variant.value))?;
        }
        self.out.raw("]")?;
        self.enum_names(name, variants, false)?;
        self.out.raw(",\"x-enumDescriptions\":[")?;
        for (index, variant) in variants.iter().enumerate() {
            if index != 0 { self.out.raw(",")?; }
            self.out.string(&variant.description)?;
        }
        self.out.raw("]")
    }

    fn enum_names(&mut self, name: &str, variants: &[EnumVariantSpec], prefixed: bool) -> Result<()> {
        self.out.raw(",\"x-enumNames\":[")?;
        for (index, variant) in variants.iter().enumerate() {
            if index != 0 { self.out.raw(",")?; }
            if prefixed {
                self.out.string_parts(&[name, ".", &variant.name])?;
            } else {
                self.out.string(&variant.name)?;
            }
        }
        self.out.raw("]")
    }
}

// URI parsing is deliberately allocation-free. HTTP server URIs use RFC 3986
// ASCII syntax; non-ASCII characters must be percent-encoded. The OpenAPI URL
// remains verbatim. AsyncAPI host is the authority's host and optional port;
// query and fragment components are not part of that host or its base pathname.
struct ServerUrl<'a> {
    host: &'a str,
    protocol: &'static str,
    pathname: &'a str,
}

fn malformed_url() -> Error {
    invalid("server URL must be an absolute, well-formed HTTP(S) URI")
}

fn parse_server_url(url: &str) -> Result<ServerUrl<'_>> {
    let (scheme, rest) = url.split_once("://").ok_or_else(malformed_url)?;
    let protocol = if scheme.eq_ignore_ascii_case("https") {
        "https"
    } else if scheme.eq_ignore_ascii_case("http") {
        "http"
    } else {
        return Err(malformed_url());
    };
    validate_uri_bytes(rest)?;
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    let host = if let Some((userinfo, host)) = authority.split_once('@') {
        if userinfo.bytes().any(|byte| !is_unreserved(byte) && !is_sub_delimiter(byte)
            && !matches!(byte, b':' | b'%'))
        {
            return Err(malformed_url());
        }
        host
    } else {
        authority
    };
    if host.starts_with('[') {
        let close = host.find(']').ok_or_else(malformed_url)?;
        let address = &host[1..close];
        if address.parse::<core::net::Ipv6Addr>().is_err() {
            return Err(malformed_url());
        }
        let suffix = &host[close + 1..];
        if !suffix.is_empty() {
            validate_port(suffix.strip_prefix(':').ok_or_else(malformed_url)?)?;
        }
    } else {
        let (name, port) = match host.split_once(':') {
            Some((name, port)) => (name, Some(port)),
            None => (host, None),
        };
        if name.is_empty() || name.bytes().any(|byte| {
            !is_unreserved(byte) && !is_sub_delimiter(byte) && byte != b'%'
        }) {
            return Err(malformed_url());
        }
        // Dotted decimal literals must actually fit the four IPv4 octets.
        if name.contains('.') && name.bytes().all(|byte| byte.is_ascii_digit() || byte == b'.')
            && name.parse::<core::net::Ipv4Addr>().is_err()
        {
            return Err(malformed_url());
        }
        if let Some(port) = port {
            validate_port(port)?;
        }
    }
    let tail = &rest[authority_end..];
    if let Some((_, fragment)) = tail.split_once('#') {
        if fragment.contains('#') {
            return Err(malformed_url());
        }
    }
    let path_end = tail.find(['?', '#']).unwrap_or(tail.len());
    let pathname = &tail[..path_end];
    Ok(ServerUrl { host, protocol, pathname })
}

fn validate_port(port: &str) -> Result<()> {
    if port.is_empty() || !port.bytes().all(|byte| byte.is_ascii_digit())
        || port.parse::<u16>().is_err()
    {
        return Err(malformed_url());
    }
    Ok(())
}

fn is_unreserved(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~')
}

fn is_sub_delimiter(byte: u8) -> bool {
    matches!(byte, b'!' | b'$' | b'&' | b'\'' | b'(' | b')' | b'*' | b'+' | b',' | b';' | b'=')
}

fn validate_uri_bytes(text: &str) -> Result<()> {
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == b'%' {
            if bytes.get(index + 1).is_none_or(|byte| !byte.is_ascii_hexdigit())
                || bytes.get(index + 2).is_none_or(|byte| !byte.is_ascii_hexdigit())
            {
                return Err(malformed_url());
            }
            index += 3;
        } else if is_unreserved(byte) || is_sub_delimiter(byte)
            || matches!(byte, b':' | b'/' | b'?' | b'#' | b'[' | b']' | b'@')
        {
            index += 1;
        } else {
            return Err(malformed_url());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{boxed::Box, format, vec};

    type Projection = fn(
        &[ServiceSpec], &TypeRegistry, Visibility<'_>, &DocumentOptions,
    ) -> Result<GeneratedDocument>;

    fn projections() -> [Projection; 2] {
        [openapi, asyncapi]
    }

    fn field(name: &str, ty: EndpointType) -> EndpointField {
        EndpointField { name: String::from(name), ty, description: String::from("field description") }
    }

    fn structure(name: &str, fields: Vec<EndpointField>) -> EndpointType {
        EndpointType::Struct { name: String::from(name), fields }
    }

    fn endpoint(name: &str, code: u32, roles: &[&str], stream: Option<EndpointType>) -> EndpointSchemaSpec {
        EndpointSchemaSpec {
            code, name: String::from(name), service: String::from("api"),
            roles: roles.iter().map(|role| String::from(*role)).collect(),
            params: vec![field("request", EndpointType::String)],
            response: vec![field("accepted", EndpointType::Boolean)],
            stream_response: stream, description: String::from("Endpoint description"),
        }
    }

    fn catalog() -> Vec<ServiceSpec> {
        vec![
            ServiceSpec {
                id: u16::MAX, name: String::from("api"), endpoints: vec![
                    endpoint("watch", u32::MAX, &["reader", "admin"], Some(EndpointType::Int64)),
                    endpoint("ping", 0, &[], None),
                    endpoint("updates", 7, &[], Some(EndpointType::String)),
                    endpoint("secret", 8, &["admin"], Some(EndpointType::Bytea)),
                ],
            },
            ServiceSpec { id: 1, name: String::from("empty"), endpoints: Vec::new() },
        ]
    }

    fn assert_error<T>(result: Result<T>, expected: ErrorCode) {
        let error = match result {
            Ok(_) => panic!("expected {expected:?}"),
            Err(error) => error,
        };
        assert_eq!(error.code, expected);
        assert_eq!(error.source, ErrorSource::Encode);
        assert_eq!((error.byte_offset, error.schema_id, error.field_id), (0, 0, 0));
        assert!(!error.detail.is_empty());
    }

    // A small test-only JSON reader verifies full document syntax, duplicate
    // object keys, decoded metadata, and reference targets without dependencies.
    #[derive(Debug, PartialEq)]
    enum Json {
        Object(Vec<(String, Json)>),
        Array(Vec<Json>),
        String(String),
        Number(String),
        Bool(bool),
        Null,
    }

    impl Json {
        fn object(&self) -> &[(String, Json)] {
            match self { Self::Object(values) => values, _ => panic!("expected object: {self:?}") }
        }

        fn array(&self) -> &[Json] {
            match self { Self::Array(values) => values, _ => panic!("expected array: {self:?}") }
        }

        fn string(&self) -> &str {
            match self { Self::String(value) => value, _ => panic!("expected string: {self:?}") }
        }

        fn get(&self, key: &str) -> Option<&Json> {
            self.object().iter().find(|(name, _)| name == key).map(|(_, value)| value)
        }

        fn at(&self, key: &str) -> &Json {
            self.get(key).unwrap_or_else(|| panic!("missing key {key}"))
        }

        fn keys(&self) -> Vec<&str> {
            self.object().iter().map(|(name, _)| name.as_str()).collect()
        }
    }

    struct Reader<'a> {
        text: &'a str,
        position: usize,
    }

    impl Reader<'_> {
        fn whitespace(&mut self) {
            while self.text.as_bytes().get(self.position).is_some_and(|byte| {
                matches!(byte, b' ' | b'\n' | b'\r' | b'\t')
            }) {
                self.position += 1;
            }
        }

        fn take(&mut self, expected: u8) {
            self.whitespace();
            assert_eq!(self.text.as_bytes().get(self.position), Some(&expected));
            self.position += 1;
        }

        fn string(&mut self) -> String {
            self.take(b'"');
            let mut result = String::new();
            loop {
                let character = self.text[self.position..].chars().next().expect("unterminated string");
                self.position += character.len_utf8();
                match character {
                    '"' => return result,
                    '\\' => {
                        let escape = self.text.as_bytes()[self.position];
                        self.position += 1;
                        result.push(match escape {
                            b'"' => '"', b'\\' => '\\', b'/' => '/',
                            b'b' => '\x08', b'f' => '\x0c', b'n' => '\n', b'r' => '\r', b't' => '\t',
                            b'u' => {
                                let end = self.position + 4;
                                let code = u32::from_str_radix(&self.text[self.position..end], 16).unwrap();
                                self.position = end;
                                char::from_u32(code).expect("generated escape is a Unicode scalar")
                            }
                            _ => panic!("invalid JSON escape"),
                        });
                    }
                    character => {
                        assert!(character >= '\u{20}');
                        result.push(character);
                    }
                }
            }
        }

        fn value(&mut self) -> Json {
            self.whitespace();
            match self.text.as_bytes()[self.position] {
                b'{' => {
                    self.position += 1;
                    self.whitespace();
                    let mut values = Vec::new();
                    if self.text.as_bytes()[self.position] != b'}' {
                        loop {
                            let key = self.string();
                            assert!(!values.iter().any(|(name, _)| name == &key), "duplicate JSON key");
                            self.take(b':');
                            values.push((key, self.value()));
                            self.whitespace();
                            if self.text.as_bytes()[self.position] == b'}' { break; }
                            self.take(b',');
                        }
                    }
                    self.take(b'}');
                    Json::Object(values)
                }
                b'[' => {
                    self.position += 1;
                    self.whitespace();
                    let mut values = Vec::new();
                    if self.text.as_bytes()[self.position] != b']' {
                        loop {
                            values.push(self.value());
                            self.whitespace();
                            if self.text.as_bytes()[self.position] == b']' { break; }
                            self.take(b',');
                        }
                    }
                    self.take(b']');
                    Json::Array(values)
                }
                b'"' => Json::String(self.string()),
                b't' => { self.literal("true"); Json::Bool(true) }
                b'f' => { self.literal("false"); Json::Bool(false) }
                b'n' => { self.literal("null"); Json::Null }
                b'-' | b'0'..=b'9' => {
                    let start = self.position;
                    if self.text.as_bytes()[self.position] == b'-' { self.position += 1; }
                    let digit_start = self.position;
                    while self.text.as_bytes().get(self.position).is_some_and(u8::is_ascii_digit) {
                        self.position += 1;
                    }
                    assert!(self.position > digit_start);
                    assert!(self.position - digit_start == 1 || self.text.as_bytes()[digit_start] != b'0');
                    let number = &self.text[start..self.position];
                    number.parse::<i64>().unwrap();
                    Json::Number(String::from(number))
                }
                _ => panic!("invalid JSON at {}", self.position),
            }
        }

        fn literal(&mut self, literal: &str) {
            assert!(self.text[self.position..].starts_with(literal));
            self.position += literal.len();
        }
    }

    fn read(document: &GeneratedDocument) -> Json {
        assert_eq!(document.media_type, "application/json");
        assert!(document.text.ends_with('\n'));
        assert!(!document.text.ends_with("\n\n"));
        let mut reader = Reader { text: &document.text, position: 0 };
        let value = reader.value();
        reader.whitespace();
        assert_eq!(reader.position, document.text.len());
        value
    }

    fn resolve<'a>(root: &'a Json, reference: &str) -> &'a Json {
        let pointer = reference.strip_prefix("#/").expect("reference must be a local pointer");
        let mut current = root;
        for token in pointer.split('/') {
            let token = token.replace("~1", "/").replace("~0", "~");
            current = match current {
                Json::Object(_) => current.at(&token),
                Json::Array(values) => &values[token.parse::<usize>().unwrap()],
                _ => panic!("reference crosses scalar: {reference}"),
            };
        }
        current
    }

    fn assert_references(root: &Json) -> usize {
        let mut pending = vec![root];
        let mut references = 0;
        while let Some(value) = pending.pop() {
            match value {
                Json::Object(values) => {
                    for (name, value) in values {
                        if name == "$ref" {
                            resolve(root, value.string());
                            references += 1;
                        }
                        pending.push(value);
                    }
                }
                Json::Array(values) => pending.extend(values),
                _ => {}
            }
        }
        references
    }

    fn request_schema<'a>(root: &'a Json, path: &str) -> &'a Json {
        root.at("paths").at(path).at("post").at("requestBody")
            .at("content").at("application/json").at("schema")
    }

    #[test]
    fn empty_catalogs_have_exact_deterministic_documents() {
        let options = DocumentOptions::default();
        let registry = TypeRegistry::default();
        let expected = [
            "{\"openapi\":\"3.1.0\",\"info\":{\"title\":\"Endpoint API\",\"version\":\"1.0.0\",\"description\":\"\"},\"paths\":{}}\n",
            "{\"asyncapi\":\"3.0.0\",\"info\":{\"title\":\"Endpoint API\",\"version\":\"1.0.0\",\"description\":\"\"},\"channels\":{},\"operations\":{}}\n",
        ];
        for (projection, expected) in projections().into_iter().zip(expected) {
            let document = projection(&[], &registry, Visibility::All, &options).unwrap();
            assert_eq!(document.text, expected);
            let json = read(&document);
            assert!(json.get("servers").is_none());
            assert_eq!(assert_references(&json), 0);
            assert_eq!(projection(&[], &registry, Visibility::Public, &options).unwrap(), document);
        }
    }

    #[test]
    fn openapi_projects_closed_request_and_immediate_response_separately() {
        let input = catalog();
        let document = openapi(&input, &TypeRegistry::default(), Visibility::All, &DocumentOptions::default()).unwrap();
        let json = read(&document);
        assert_eq!(json.at("openapi").string(), OPENAPI_VERSION);
        assert_eq!(json.at("paths").keys(), ["/rpc/api/watch", "/rpc/api/ping", "/rpc/api/updates", "/rpc/api/secret"]);
        let operation = json.at("paths").at("/rpc/api/watch").at("post");
        assert_eq!(operation.at("operationId").string(), tool_name("api", "watch").unwrap());
        assert_eq!(operation.at("description").string(), input[0].endpoints[0].description);
        assert_eq!(operation.at("tags").array(), [Json::String(String::from("api"))]);
        assert_eq!(operation.at("x-jmax-code"), &Json::Number(String::from("4294967295")));
        assert_eq!(operation.at("x-jmax-roles").array(), [Json::String(String::from("reader")), Json::String(String::from("admin"))]);
        assert_eq!(operation.at("requestBody").at("required"), &Json::Bool(true));
        let request = request_schema(&json, "/rpc/api/watch");
        assert_eq!(request.at("additionalProperties"), &Json::Bool(false));
        assert_eq!(request.at("properties").keys(), ["request"]);
        assert_eq!(request.at("required").array(), [Json::String(String::from("request"))]);
        let response = operation.at("responses").at("200").at("content").at("application/json").at("schema");
        assert_eq!(response.at("properties").keys(), ["accepted"]);
        assert_eq!(response.at("properties").at("accepted").at("type").string(), "boolean");
        let stream = operation.at("x-jmax-stream-schema");
        assert_eq!(stream.at("type").string(), "integer");
        assert_eq!(stream.at("minimum"), &Json::Number(String::from("-9223372036854775808")));
        assert_eq!(stream.at("maximum"), &Json::Number(String::from("9223372036854775807")));
        assert!(json.at("paths").at("/rpc/api/ping").at("post").get("x-jmax-stream-schema").is_none());
        assert!(!document.text.contains("security"));
        assert_eq!(assert_references(&json), 0);
    }

    #[test]
    fn asyncapi_only_exposes_streams_with_resolving_channel_and_message_references() {
        let input = catalog();
        let document = asyncapi(&input, &TypeRegistry::default(), Visibility::All, &DocumentOptions::default()).unwrap();
        let json = read(&document);
        assert_eq!(json.at("asyncapi").string(), ASYNCAPI_VERSION);
        let keys = ["api.watch", "api.updates", "api.secret"];
        assert_eq!(json.at("channels").keys(), keys);
        assert_eq!(json.at("operations").keys(), keys);
        for name in keys {
            let channel = json.at("channels").at(name);
            assert_eq!(channel.at("messages").keys(), ["result"]);
            let result = channel.at("messages").at("result");
            assert_eq!(result.at("name").string(), name);
            assert_eq!(result.at("contentType").string(), "application/json");
            assert_eq!(result.at("payload").at("$schema").string(), JSON_SCHEMA_DIALECT);
            let operation = json.at("operations").at(name);
            assert_eq!(operation.at("action").string(), "send");
            assert!(core::ptr::eq(resolve(&json, operation.at("channel").at("$ref").string()), channel));
            assert!(core::ptr::eq(resolve(&json, operation.at("messages").array()[0].at("$ref").string()), result));
        }
        assert_eq!(json.at("channels").at("api.watch").at("address").string(), "/rpc/api/watch");
        assert_eq!(json.at("channels").at("api.watch").at("messages").at("result").at("payload").at("type").string(), "integer");
        assert_eq!(assert_references(&json), 6);
        assert!(!document.text.contains("accepted"));
        assert!(!document.text.contains("request"));
        assert!(!document.text.contains("api.ping"));
        assert!(!document.text.contains("security"));
    }

    #[test]
    fn visibility_is_exact_preserves_order_and_never_leaks_hidden_metadata() {
        let mut input = catalog();
        input[0].endpoints[3].description = String::from("hidden description sentinel");
        input[0].endpoints[3].params[0].name = String::from("hidden_field_sentinel");
        let roles = vec![String::from("reader")];
        for projection in projections() {
            let public = projection(&input, &TypeRegistry::default(), Visibility::Public, &DocumentOptions::default()).unwrap();
            let permitted = projection(&input, &TypeRegistry::default(), Visibility::Roles(&roles), &DocumentOptions::default()).unwrap();
            for document in [&public, &permitted] {
                assert!(!document.text.contains("hidden description sentinel"));
                assert!(!document.text.contains("hidden_field_sentinel"));
                assert!(!document.text.contains("secret"));
                read(document);
            }
            assert!(!public.text.contains("api.watch"));
            assert!(permitted.text.contains("api.watch"));
            let differently_cased = vec![String::from("Reader")];
            assert_eq!(projection(&input, &TypeRegistry::default(), Visibility::Roles(&differently_cased), &DocumentOptions::default()).unwrap(), public);
            assert_eq!(projection(&input, &TypeRegistry::default(), Visibility::Roles(&[]), &DocumentOptions::default()).unwrap(), public);
        }
        let all_unary = vec![ServiceSpec {
            id: 1, name: String::from("api"), endpoints: vec![endpoint("ping", 1, &[], None)],
        }];
        let json = read(&asyncapi(&all_unary, &TypeRegistry::default(), Visibility::All, &DocumentOptions::default()).unwrap());
        assert!(json.at("channels").object().is_empty());
        assert!(json.at("operations").object().is_empty());
    }

    #[test]
    fn every_generated_byte_is_charged_including_unicode_references_and_final_newline() {
        let mut input = catalog();
        input[0].endpoints[0].stream_response = Some(EndpointType::StructRef(String::from("Node")));
        let registry = TypeRegistry { structs: vec![structure("Node", vec![field("next", EndpointType::Optional(Box::new(EndpointType::StructRef(String::from("Node")))) )])], enums: Vec::new() };
        let mut options = DocumentOptions {
            title: String::from("é雪\n\"\\"), description: String::from("\0\u{1f}"),
            server_url: Some(String::from("https://example.com/base")), ..DocumentOptions::default()
        };
        for projection in projections() {
            options.max_output_bytes = 1_000_000;
            let expected = projection(&input, &registry, Visibility::All, &options).unwrap();
            let json = read(&expected);
            assert!(assert_references(&json) > 0);
            assert_eq!(json.at("info").at("title").string(), options.title);
            assert_eq!(json.at("info").at("description").string(), options.description);
            options.max_output_bytes = expected.text.len() as u32;
            assert_eq!(projection(&input, &registry, Visibility::All, &options).unwrap(), expected);
            options.max_output_bytes -= 1;
            assert_error(projection(&input, &registry, Visibility::All, &options), ErrorCode::PayloadTooLarge);
            for limit in [0, 1, 64] {
                options.max_output_bytes = limit;
                assert_error(projection(&input, &registry, Visibility::All, &options), ErrorCode::PayloadTooLarge);
                assert_error(projection(&[], &registry, Visibility::All, &options), ErrorCode::PayloadTooLarge);
            }
        }
    }

    #[test]
    fn recursive_definitions_resolve_at_every_distinct_embedded_schema_location() {
        let node = structure("Node", vec![
            field("next", EndpointType::Optional(Box::new(EndpointType::StructRef(String::from("Node"))))),
            field("links", EndpointType::StructTable { struct_ref: String::from("Peer") }),
        ]);
        let peer = structure("Peer", vec![field("owner", EndpointType::StructRef(String::from("Node")))]);
        let registry = TypeRegistry { structs: vec![node, peer], enums: Vec::new() };
        let mut input = catalog();
        for endpoint in &mut input[0].endpoints {
            endpoint.params = vec![field("node", EndpointType::StructRef(String::from("Node")))];
            endpoint.response = vec![field("node", EndpointType::StructRef(String::from("Peer")))];
            endpoint.stream_response = Some(EndpointType::StructRef(String::from("Node")));
        }
        for projection in projections() {
            let document = projection(&input, &registry, Visibility::All, &DocumentOptions::default()).unwrap();
            let json = read(&document);
            assert!(assert_references(&json) >= 16);
            assert!(!document.text.contains("\"$ref\":\"#/$defs/"));
            assert!(document.text.contains("/$defs/Node"));
            assert!(document.text.contains("/$defs/Peer"));
            assert!(document.text.len() < 100_000);
        }
        let document = openapi(&input, &registry, Visibility::Public, &DocumentOptions::default()).unwrap();
        assert!(document.text.contains("#/paths/~1rpc~1api~1ping/post/requestBody/content/application~1json/schema/$defs/Node"));
        assert!(document.text.contains("#/paths/~1rpc~1api~1ping/post/responses/200/content/application~1json/schema/$defs/Peer"));
        assert!(document.text.contains("#/paths/~1rpc~1api~1ping/post/x-jmax-stream-schema/$defs/Node"));
        let document = asyncapi(&input, &registry, Visibility::Public, &DocumentOptions::default()).unwrap();
        assert!(document.text.contains("#/channels/api.ping/messages/result/payload/$defs/Node"));
    }

    #[test]
    fn inline_definitions_are_catalog_wide_and_only_reachable_definitions_are_emitted() {
        let mut input = catalog();
        input[0].endpoints[0].params = vec![field("record", structure("Record", vec![field("id", EndpointType::UInt32)]))];
        input[0].endpoints[0].description = String::from("hidden endpoint sentinel");
        input[0].endpoints[2].stream_response = Some(EndpointType::StructRef(String::from("Record")));
        input[0].endpoints[2].params = vec![field("record", EndpointType::StructRef(String::from("Record")))];
        let registry = TypeRegistry { structs: vec![structure("Unused", vec![field("unused_sentinel", EndpointType::String)])], enums: Vec::new() };
        for projection in projections() {
            let document = projection(&input, &registry, Visibility::Public, &DocumentOptions::default()).unwrap();
            let json = read(&document);
            assert!(assert_references(&json) > 0);
            assert!(document.text.contains("\"Record\""));
            assert!(!document.text.contains("Unused"));
            assert!(!document.text.contains("unused_sentinel"));
            assert!(!document.text.contains("hidden endpoint sentinel"));
        }
    }

    #[test]
    fn all_wire_types_match_the_shared_json_schema_projection() {
        let enumeration = EndpointType::Enum {
            name: String::from("State"), variants: vec![
                EnumVariantSpec { name: String::from("Min"), value: i32::MIN, description: String::from("minimum") },
                EnumVariantSpec { name: String::from("Max"), value: i32::MAX, description: String::from("maximum") },
            ],
        };
        let registry = TypeRegistry { structs: vec![structure("Record", vec![field("id", EndpointType::UUID)])], enums: vec![enumeration.clone()] };
        let types = vec![
            EndpointType::UInt32, EndpointType::Int32, EndpointType::Int64, EndpointType::Float64,
            EndpointType::Boolean, EndpointType::String, EndpointType::Bytea, EndpointType::UUID,
            EndpointType::NanoId { len: 1 }, EndpointType::NanoId { len: u8::MAX }, EndpointType::IpAddr,
            EndpointType::Object, EndpointType::Unit, EndpointType::TimeStampMs, EndpointType::BlockchainDecimal,
            EndpointType::BlockchainAddress, EndpointType::BlockchainTransactionHash,
            EndpointType::Vec(Box::new(EndpointType::Optional(Box::new(EndpointType::Int64)))),
            EndpointType::Optional(Box::new(EndpointType::String)), EndpointType::StructRef(String::from("Record")),
            EndpointType::StructTable { struct_ref: String::from("Record") },
            registry.structs[0].clone(), enumeration,
            EndpointType::EnumRef { name: String::from("State"), prefixed_name: true },
        ];
        let options = DocumentOptions::default();
        for ty in types {
            let input = vec![ServiceSpec {
                id: 0, name: String::from("api"), endpoints: vec![endpoint("events", 0, &[], Some(ty.clone()))],
            }];
            let expected = json_schema::type_schema(&ty, &registry, &options).unwrap();
            for projection in projections() {
                let document = projection(&input, &registry, Visibility::All, &options).unwrap();
                let json = read(&document);
                assert_references(&json);
                let actual = if json.get("paths").is_some() {
                    json.at("paths").at("/rpc/api/events").at("post").at("x-jmax-stream-schema")
                } else {
                    json.at("channels").at("api.events").at("messages").at("result").at("payload")
                };
                let mut expected_reader = Reader { text: &expected.text, position: 0 };
                let expected_json = expected_reader.value();
                // The schemas differ only in the absolute locations of refs.
                assert_eq!(normalize_refs(actual), normalize_refs(&expected_json));
            }
        }
    }

    fn normalize_refs(value: &Json) -> Json {
        match value {
            Json::Object(values) => Json::Object(values.iter().map(|(key, value)| {
                let value = if key == "$ref" {
                    let name = value.string().rsplit('/').next().unwrap();
                    Json::String(format!("#/$defs/{name}"))
                } else { normalize_refs(value) };
                (key.clone(), value)
            }).collect()),
            Json::Array(values) => Json::Array(values.iter().map(normalize_refs).collect()),
            Json::String(value) => Json::String(value.clone()),
            Json::Number(value) => Json::Number(value.clone()),
            Json::Bool(value) => Json::Bool(*value),
            Json::Null => Json::Null,
        }
    }

    #[test]
    fn malformed_catalogs_are_rejected_before_any_visibility_or_stream_filtering() {
        for projection in projections() {
            let mut input = catalog();
            input[0].endpoints[3].params[0].ty = EndpointType::StructRef(String::from("Missing"));
            assert_error(projection(&input, &TypeRegistry::default(), Visibility::Public, &DocumentOptions::default()), ErrorCode::ConstraintViolation);
            input[0].endpoints[3].params[0].ty = EndpointType::String;
            input[0].endpoints[3].stream_response = Some(EndpointType::NanoId { len: 0 });
            assert_error(projection(&input, &TypeRegistry::default(), Visibility::Public, &DocumentOptions::default()), ErrorCode::ConstraintViolation);
            input[0].endpoints[3].stream_response = None;
            input[0].endpoints[3].code = 0;
            assert_error(projection(&input, &TypeRegistry::default(), Visibility::Public, &DocumentOptions::default()), ErrorCode::DuplicateObjectKey);
            input[0].endpoints[3].code = 8;
            input[0].endpoints[3].service = String::from("wrong");
            assert_error(projection(&input, &TypeRegistry::default(), Visibility::Public, &DocumentOptions::default()), ErrorCode::ConstraintViolation);
            input[0].endpoints[3].service = String::from("api");
            input[0].endpoints[3].params.push(field("request", EndpointType::String));
            assert_error(projection(&input, &TypeRegistry::default(), Visibility::Public, &DocumentOptions::default()), ErrorCode::DuplicateObjectKey);
        }
    }

    #[test]
    fn registry_kinds_duplicates_conflicts_and_unresolved_edges_use_canonical_errors() {
        let duplicate_enum = EndpointType::Enum {
            name: String::from("State"), variants: vec![
                EnumVariantSpec { name: String::from("A"), value: 1, description: String::new() },
                EnumVariantSpec { name: String::from("B"), value: 1, description: String::new() },
            ],
        };
        let vectors = vec![
            (TypeRegistry { structs: vec![EndpointType::String], enums: Vec::new() }, ErrorCode::ConstraintViolation),
            (TypeRegistry { structs: vec![structure("bad-name", Vec::new())], enums: Vec::new() }, ErrorCode::ConstraintViolation),
            (TypeRegistry { structs: vec![structure("Same", Vec::new()), structure("Same", Vec::new())], enums: Vec::new() }, ErrorCode::DuplicateObjectKey),
            (TypeRegistry { structs: Vec::new(), enums: vec![structure("Wrong", Vec::new())] }, ErrorCode::ConstraintViolation),
            (TypeRegistry { structs: Vec::new(), enums: vec![duplicate_enum] }, ErrorCode::DuplicateObjectKey),
            (TypeRegistry { structs: vec![structure("Unresolved", vec![field("x", EndpointType::StructRef(String::from("Missing")))])], enums: Vec::new() }, ErrorCode::ConstraintViolation),
            (TypeRegistry { structs: vec![structure("WrongKind", vec![field("x", EndpointType::EnumRef { name: String::from("WrongKind"), prefixed_name: false })])], enums: Vec::new() }, ErrorCode::ConstraintViolation),
            (TypeRegistry { structs: vec![structure("DuplicateFields", vec![field("x", EndpointType::String), field("x", EndpointType::Unit)])], enums: Vec::new() }, ErrorCode::DuplicateObjectKey),
        ];
        for projection in projections() {
            for (registry, code) in &vectors {
                assert_error(projection(&[], registry, Visibility::Public, &DocumentOptions::default()), *code);
            }
            let registered = structure("Record", vec![field("x", EndpointType::Int32)]);
            let registry = TypeRegistry { structs: vec![registered.clone()], enums: Vec::new() };
            let mut input = catalog();
            input[0].endpoints[0].params = vec![field("record", registered)];
            assert!(projection(&input, &registry, Visibility::Public, &DocumentOptions::default()).is_ok());
            input[0].endpoints[0].params = vec![field("record", structure("Record", vec![field("x", EndpointType::String)]))];
            assert_error(projection(&input, &registry, Visibility::Public, &DocumentOptions::default()), ErrorCode::DuplicateObjectKey);
            input[0].endpoints[0].params = vec![field("record", structure("Local", Vec::new()))];
            input[0].endpoints[1].params = vec![field("record", structure("Local", Vec::new()))];
            assert_error(projection(&input, &TypeRegistry::default(), Visibility::All, &DocumentOptions::default()), ErrorCode::DuplicateObjectKey);
        }
    }

    #[test]
    fn inline_depth_boundaries_and_large_limits_do_not_overflow() {
        for projection in projections() {
            let mut options = DocumentOptions { max_type_depth: 0, ..DocumentOptions::default() };
            assert_error(projection(&[], &TypeRegistry::default(), Visibility::All, &options), ErrorCode::ConstraintViolation);
            options.max_type_depth = 1;
            let mut input = catalog();
            assert!(projection(&input, &TypeRegistry::default(), Visibility::All, &options).is_ok());
            input[0].endpoints[3].stream_response = Some(EndpointType::Optional(Box::new(EndpointType::Unit)));
            assert_error(projection(&input, &TypeRegistry::default(), Visibility::Public, &options), ErrorCode::DepthLimitExceeded);
            options.max_type_depth = 2;
            assert!(projection(&input, &TypeRegistry::default(), Visibility::Public, &options).is_ok());
            options.max_type_depth = u16::MAX;
            assert!(projection(&input, &TypeRegistry::default(), Visibility::All, &options).is_ok());
        }
        let options = DocumentOptions { max_type_depth: u16::MAX, ..DocumentOptions::default() };
        let registry = TypeRegistry::default();
        let mut context = Context::registry(&registry, options.max_type_depth).unwrap();
        let ty = EndpointType::Optional(Box::new(EndpointType::Unit));
        assert_error(context.visit(Visit::Type(&ty, u32::from(u16::MAX), false)), ErrorCode::DepthLimitExceeded);
    }

    #[test]
    fn deeply_nested_structures_use_explicit_stacks_and_named_cycles_do_not_add_depth() {
        let mut ty = EndpointType::Unit;
        for _ in 0..256 { ty = EndpointType::Vec(Box::new(ty)); }
        let input = vec![ServiceSpec { id: 0, name: String::from("api"), endpoints: vec![endpoint("deep", 0, &[], Some(ty))] }];
        let options = DocumentOptions { max_type_depth: 257, ..DocumentOptions::default() };
        for projection in projections() {
            let document = projection(&input, &TypeRegistry::default(), Visibility::All, &options).unwrap();
            assert_eq!(document.text.matches("\"type\":\"array\"").count(), 256);
            let shallow = DocumentOptions { max_type_depth: 256, ..options.clone() };
            assert_error(projection(&input, &TypeRegistry::default(), Visibility::All, &shallow), ErrorCode::DepthLimitExceeded);
        }
        let registry = TypeRegistry { structs: vec![structure("Cycle", vec![field("next", EndpointType::StructRef(String::from("Cycle")))])], enums: Vec::new() };
        let options = DocumentOptions { max_type_depth: 2, ..DocumentOptions::default() };
        let input = vec![ServiceSpec { id: 0, name: String::from("api"), endpoints: vec![endpoint("cycle", 0, &[], Some(EndpointType::StructRef(String::from("Cycle"))))] }];
        for projection in projections() {
            let json = read(&projection(&input, &registry, Visibility::All, &options).unwrap());
            assert!(assert_references(&json) >= 2);
        }
    }

    #[test]
    fn valid_server_urls_preserve_openapi_urls_and_split_asyncapi_authorities() {
        for (url, host, protocol, pathname) in [
            ("https://example.com", "example.com", "https", ""),
            ("http://localhost:0/", "localhost:0", "http", "/"),
            ("HTTPS://api.example.com:65535/base/v1?key=value#section", "api.example.com:65535", "https", "/base/v1"),
            ("http://127.0.0.1:8080/rpc", "127.0.0.1:8080", "http", "/rpc"),
            ("https://[2001:db8::1]:443/base", "[2001:db8::1]:443", "https", "/base"),
            ("http://[::ffff:192.0.2.1]/", "[::ffff:192.0.2.1]", "http", "/"),
            ("https://user:p%40ss@example.com/a%20b?q=%22#x", "example.com", "https", "/a%20b"),
            ("http://example.com?query#fragment", "example.com", "http", ""),
        ] {
            let options = DocumentOptions { server_url: Some(String::from(url)), ..DocumentOptions::default() };
            let registry = TypeRegistry::default();
            let json = read(&openapi(&[], &registry, Visibility::All, &options).unwrap());
            assert_eq!(json.at("servers").array()[0].at("url").string(), url);
            let json = read(&asyncapi(&[], &registry, Visibility::All, &options).unwrap());
            let server = json.at("servers").at("default");
            assert_eq!(server.at("host").string(), host);
            assert_eq!(server.at("protocol").string(), protocol);
            if pathname.is_empty() {
                assert!(server.get("pathname").is_none());
            } else {
                assert_eq!(server.at("pathname").string(), pathname);
            }
        }
    }

    #[test]
    fn malformed_or_unsupported_server_urls_are_always_rejected() {
        for url in [
            "", "/relative", "//example.com", "https:example.com", "https:///path", "https://",
            "ws://example.com", "ftp://example.com", "http://:80", "https://example.com:",
            "http://example.com:65536", "http://example.com:-1", "http://example.com:+80",
            "http://example.com:abc", "http://example.com:80:90", "http://bad host/",
            "http://example.com/\n", "http://example.com/\0", "http://example.com/\\bad",
            "http://example.com/%", "http://example.com/%0", "http://example.com/%xy",
            "http://[::1", "http://[]/", "http://[invalid]/", "http://[::1]trailing/",
            "http://::1/", "http://999.1.1.1/", "http://user@@example.com/", "http://example.com/#a#b",
        ] {
            let options = DocumentOptions { server_url: Some(String::from(url)), max_output_bytes: 0, ..DocumentOptions::default() };
            for projection in projections() {
                assert_error(projection(&[], &TypeRegistry::default(), Visibility::Public, &options), ErrorCode::ConstraintViolation);
            }
        }
    }

    #[test]
    fn full_json_string_escaping_preserves_every_control_and_unicode_value() {
        let controls: String = (0..=31).map(|value| char::from_u32(value).unwrap()).collect();
        let text = format!("{controls}\"\\/é雪😀");
        let mut input = catalog();
        input[0].endpoints[0].description = text.clone();
        input[0].endpoints[0].roles = vec![text.clone()];
        input[0].endpoints[0].params[0].name = text.clone();
        input[0].endpoints[0].params[0].description = text.clone();
        let options = DocumentOptions { title: text.clone(), version: text.clone(), description: text.clone(), ..DocumentOptions::default() };
        for projection in projections() {
            let json = read(&projection(&input, &TypeRegistry::default(), Visibility::All, &options).unwrap());
            for name in ["title", "version", "description"] { assert_eq!(json.at("info").at(name).string(), text); }
        }
        let json = read(&openapi(&input, &TypeRegistry::default(), Visibility::All, &options).unwrap());
        assert_eq!(json.at("paths").at("/rpc/api/watch").at("post").at("description").string(), text);
        let properties = request_schema(&json, "/rpc/api/watch").at("properties");
        assert_eq!(properties.keys(), [text.as_str()]);
        assert_eq!(properties.at(&text).at("description").string(), text);
        let mut out = Writer::new(1024);
        out.reference(&["a/b~c", "quote\"\\\n雪"] ).unwrap();
        assert_eq!(out.text, "\"$ref\":\"#/a~1b~0c/quote\\\"\\\\\\n雪\"");
    }

    #[test]
    fn writer_enforces_append_bounds_and_exact_signed_integer_text() {
        for (value, expected) in [
            (i64::MIN, "-9223372036854775808"), (i64::MAX, "9223372036854775807"),
            (i64::from(u32::MAX), "4294967295"), (-1, "-1"), (0, "0"), (10, "10"),
        ] {
            let mut out = Writer::new(expected.len() as u32);
            out.integer(value).unwrap();
            assert_eq!(out.text, expected);
            let mut out = Writer::new(expected.len() as u32 - 1);
            assert_error(out.integer(value), ErrorCode::PayloadTooLarge);
            assert!(out.text.is_empty());
        }
        let mut out = Writer::new(4);
        out.raw("é").unwrap();
        assert_error(out.raw("雪"), ErrorCode::PayloadTooLarge);
        assert_eq!(out.text, "é");
        out.raw("é").unwrap();
        assert_eq!(out.text.len(), 4);
        assert_error(out.raw("x"), ErrorCode::PayloadTooLarge);
        out.raw("").unwrap();
        let mut values: Vec<u64> = Vec::new();
        assert!(values.try_reserve(usize::MAX).is_err());
        assert_eq!(allocation_error().code, ErrorCode::BudgetExhausted);
    }

    #[test]
    fn stable_input_preserves_service_endpoint_role_and_field_order_without_mutation() {
        let mut input = catalog();
        let mut second = endpoint("last", 99, &[], Some(EndpointType::Boolean));
        second.service = String::from("second");
        input.push(ServiceSpec { id: 2, name: String::from("second"), endpoints: vec![second] });
        input[0].endpoints[0].params = vec![
            field("z", EndpointType::Optional(Box::new(EndpointType::String))),
            field("a", EndpointType::Vec(Box::new(EndpointType::Optional(Box::new(EndpointType::Int32))))),
            field("null", EndpointType::Unit),
        ];
        let snapshot = input.clone();
        let registry = TypeRegistry::default();
        let options = DocumentOptions::default();
        for projection in projections() {
            let expected = projection(&input, &registry, Visibility::All, &options).unwrap();
            read(&expected);
            for _ in 0..3 {
                assert_eq!(projection(&input, &registry, Visibility::All, &options).unwrap(), expected);
            }
        }
        let json = read(&openapi(&input, &registry, Visibility::All, &options).unwrap());
        assert_eq!(json.at("paths").keys(), ["/rpc/api/watch", "/rpc/api/ping", "/rpc/api/updates", "/rpc/api/secret", "/rpc/second/last"]);
        let request = request_schema(&json, "/rpc/api/watch");
        assert_eq!(request.at("properties").keys(), ["z", "a", "null"]);
        assert_eq!(request.at("required").array(), [Json::String(String::from("a")), Json::String(String::from("null"))]);
        assert_eq!(request.at("properties").at("z").at("anyOf").array()[1].at("type").string(), "null");
        let json = read(&asyncapi(&input, &registry, Visibility::All, &options).unwrap());
        assert_eq!(json.at("channels").keys(), ["api.watch", "api.updates", "api.secret", "second.last"]);
        assert_eq!(input, snapshot);
    }
}
