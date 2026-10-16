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
