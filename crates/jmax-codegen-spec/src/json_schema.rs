//! Bounded JSON Schema 2020-12 projections and ordered MCP tool catalogs.
//!
//! Validation and serialization use explicit stacks. Named graphs are validated
//! without expanding references, and each document publishes only reachable
//! definitions. No input values are cloned during validation.

use super::*;

/// Validates definition kinds, names, inline depth, and every named reference.
///
/// A type root has depth one; owned children increase depth. Named references
/// are leaves, so mutually recursive definitions are legal. Equal inline copies
/// of registered definitions are permitted; other repeated declarations fail.
pub fn validate_registry(registry: &TypeRegistry, max_depth: u16) -> Result<()> {
    Context::registry(registry, max_depth).map(|_| ())
}

/// Projects one type into a self-contained, deterministic JSON Schema document.
pub fn type_schema(
    ty: &EndpointType,
    registry: &TypeRegistry,
    options: &DocumentOptions,
) -> Result<GeneratedDocument> {
    let mut context = Context::registry(registry, options.max_type_depth)?;
    context.visit(Visit::Type(ty, 1, false))?;
    context.resolve()?;
    document(&context, Root::Type(ty), options.max_output_bytes)
}

/// Projects params or the immediate response, independently of stream results.
/// The entire endpoint is validated before choosing the requested direction.
pub fn endpoint_schema(
    endpoint: &EndpointSchemaSpec,
    direction: SchemaDirection,
    registry: &TypeRegistry,
    options: &DocumentOptions,
) -> Result<GeneratedDocument> {
    let mut context = Context::registry(registry, options.max_type_depth)?;
    context.endpoint(endpoint)?;
    context.resolve()?;
    document(&context, Root::Fields(projected_fields(endpoint, direction)), options.max_output_bytes)
}

/// Returns tools in service/endpoint input order after full catalog validation.
///
/// The aggregate UTF-8 bytes of names, descriptions, and both complete schemas
/// must fit the output budget. Hidden endpoints are validated but never emitted.
pub fn mcp_tools(
    services: &[ServiceSpec],
    registry: &TypeRegistry,
    visibility: Visibility<'_>,
    options: &DocumentOptions,
) -> Result<Vec<ToolSpec>> {
    validate_services(services)?;
    let mut context = Context::registry(registry, options.max_type_depth)?;
    for service in services {
        for endpoint in &service.endpoints {
            context.endpoint(endpoint)?;
        }
    }
    context.resolve()?;

    let mut remaining = options.max_output_bytes as usize;
    let mut tools = Vec::new();
    for service in services {
        for endpoint in &service.endpoints {
            if !visibility.includes(endpoint) {
                continue;
            }
            // Check lengths before allocating even the tool name or description.
            let name_bytes = service.name.len().checked_add(endpoint.name.len())
                .and_then(|length| length.checked_add(1))
                .ok_or_else(output_error)?;
            charge(&mut remaining, name_bytes)?;
            charge(&mut remaining, endpoint.description.len())?;
            let input_schema = document(&context, Root::Fields(&endpoint.params), remaining as u32)?;
            charge(&mut remaining, input_schema.text.len())?;
            let output_schema = document(&context, Root::Fields(&endpoint.response), remaining as u32)?;
            charge(&mut remaining, output_schema.text.len())?;
            let tool = ToolSpec {
                name: tool_name(&service.name, &endpoint.name)?,
                description: copy_text(&endpoint.description)?,
                endpoint_code: endpoint.code,
                input_schema,
                output_schema,
            };
            push(&mut tools, tool)?;
        }
    }
    Ok(tools)
}

fn projected_fields(endpoint: &EndpointSchemaSpec, direction: SchemaDirection) -> &[EndpointField] {
    match direction {
        SchemaDirection::Params => &endpoint.params,
        SchemaDirection::Response => &endpoint.response,
    }
}

fn allocation_error() -> Error {
    spec_error(ErrorCode::BudgetExhausted, "schema allocation failed")
}

fn output_error() -> Error {
    spec_error(ErrorCode::PayloadTooLarge, "schema output exceeds byte limit")
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

fn copy_text(value: &str) -> Result<String> {
    let mut copied = String::new();
    copied.try_reserve_exact(value.len()).map_err(|_| allocation_error())?;
    copied.push_str(value);
    Ok(copied)
}

fn charge(remaining: &mut usize, bytes: usize) -> Result<()> {
    *remaining = remaining.checked_sub(bytes).ok_or_else(output_error)?;
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

    fn reference(&mut self, name: &str) -> Result<()> {
        self.raw("\"$ref\":\"#/$defs/")?;
        // Escape JSON Pointer tokens before JSON string escaping. Public names
        // are ASCII identifiers, but keep the pointer operation correct itself.
        let mut start = 0;
        for (index, byte) in name.bytes().enumerate() {
            if matches!(byte, b'~' | b'/') {
                self.escaped(&name[start..index])?;
                self.raw(if byte == b'~' { "~0" } else { "~1" })?;
                start = index + 1;
            }
        }
        self.escaped(&name[start..])?;
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

struct Renderer<'a, 'c> {
    context: &'c Context<'a>,
    definitions: Vec<&'a EndpointType>,
    out: Writer,
}

fn document<'a>(context: &Context<'a>, root: Root<'a>, limit: u32) -> Result<GeneratedDocument> {
    let mut renderer = Renderer { context, definitions: Vec::new(), out: Writer::new(limit) };
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
    renderer.out.raw("}\n")?;
    GeneratedDocument::new("application/schema+json", renderer.out.text, limit)
}

impl<'a> Renderer<'a, '_> {
    fn reference(&mut self, name: &str, kind: Kind) -> Result<()> {
        let ty = self.context.definition(name, kind)?;
        if !self.definitions.iter().any(|previous| core::ptr::eq(*previous, ty)) {
            push(&mut self.definitions, ty)?;
        }
        self.out.reference(name)
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

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{boxed::Box, format, vec};

    fn field(name: &str, ty: EndpointType) -> EndpointField {
        EndpointField { name: String::from(name), ty, description: String::from("field") }
    }

    fn structure(name: &str, fields: Vec<EndpointField>) -> EndpointType {
        EndpointType::Struct { name: String::from(name), fields }
    }

    fn enumeration(name: &str) -> EndpointType {
        EndpointType::Enum {
            name: String::from(name),
            variants: vec![
                EnumVariantSpec { name: String::from("Low"), value: i32::MIN, description: String::from("minimum") },
                EnumVariantSpec { name: String::from("High"), value: i32::MAX, description: String::from("maximum") },
            ],
        }
    }

    fn endpoint(name: &str, code: u32, roles: &[&str]) -> EndpointSchemaSpec {
        EndpointSchemaSpec {
            code, name: String::from(name), service: String::from("api"),
            roles: roles.iter().map(|role| String::from(*role)).collect(),
            params: vec![field("request", EndpointType::String)],
            response: vec![field("accepted", EndpointType::Boolean)],
            stream_response: None, description: String::from("é endpoint"),
        }
    }

    fn catalog() -> Vec<ServiceSpec> {
        vec![
            ServiceSpec {
                id: u16::MAX, name: String::from("api"),
                endpoints: vec![
                    endpoint("read", 0, &["reader", "admin"]),
                    endpoint("ping", u32::MAX, &[]),
                    endpoint("write", 7, &["admin"]),
                ],
            },
            ServiceSpec { id: 1, name: String::from("empty"), endpoints: Vec::new() },
        ]
    }

    fn assert_error<T>(result: Result<T>, expected: ErrorCode) -> Error {
        let error = match result {
            Ok(_) => panic!("expected {expected:?}"),
            Err(error) => error,
        };
        assert_eq!(error.code, expected);
        assert_eq!(error.source, ErrorSource::Encode);
        assert_eq!((error.byte_offset, error.schema_id, error.field_id), (0, 0, 0));
        error
    }

    fn wrapped(body: &str) -> String {
        format!("{{\"$schema\":\"{JSON_SCHEMA_DIALECT}\",{body}}}\n")
    }

    fn schema(ty: &EndpointType) -> GeneratedDocument {
        type_schema(ty, &TypeRegistry::default(), &DocumentOptions::default()).unwrap()
    }

    fn primitive_vectors() -> Vec<(EndpointType, &'static str)> {
        vec![
            (EndpointType::UInt32, r#""type":"integer","minimum":0,"maximum":4294967295"#),
            (EndpointType::Int32, r#""type":"integer","minimum":-2147483648,"maximum":2147483647"#),
            (EndpointType::Int64, r#""type":"integer","minimum":-9223372036854775808,"maximum":9223372036854775807"#),
            (EndpointType::Float64, r#""type":"number""#),
            (EndpointType::Boolean, r#""type":"boolean""#),
            (EndpointType::String, r#""type":"string""#),
            (EndpointType::Bytea, r#""type":"string","contentEncoding":"base64""#),
            (EndpointType::UUID, r#""type":"string","format":"uuid""#),
            (EndpointType::NanoId { len: 1 }, r#""type":"string","minLength":1,"maxLength":1,"pattern":"^[A-Za-z0-9_-]+$""#),
            (EndpointType::NanoId { len: u8::MAX }, r#""type":"string","minLength":255,"maxLength":255,"pattern":"^[A-Za-z0-9_-]+$""#),
            (EndpointType::IpAddr, r#""anyOf":[{"type":"string","format":"ipv4"},{"type":"string","format":"ipv6"}]"#),
            (EndpointType::Object, r#""type":"object""#),
            (EndpointType::Unit, r#""type":"null""#),
            (EndpointType::Vec(Box::new(EndpointType::Unit)), r#""type":"array","items":{"type":"null"}"#),
            (EndpointType::Optional(Box::new(EndpointType::UInt32)), r#""anyOf":[{"type":"integer","minimum":0,"maximum":4294967295},{"type":"null"}]"#),
            (EndpointType::TimeStampMs, r#""type":"integer","minimum":-9223372036854775808,"maximum":9223372036854775807,"x-unit":"milliseconds""#),
            (EndpointType::BlockchainDecimal, r#""type":"string","pattern":"^-?[0-9]+(\\.[0-9]+)?$""#),
            (EndpointType::BlockchainAddress, r#""type":"string","minLength":1,"x-jmax-type":"BlockchainAddress""#),
            (EndpointType::BlockchainTransactionHash, r#""type":"string","minLength":1,"x-jmax-type":"BlockchainTransactionHash""#),
        ]
    }

    #[test]
    fn every_primitive_and_wrapper_has_exact_stable_schema_bytes() {
        for (ty, body) in primitive_vectors() {
            let generated = schema(&ty);
            assert_eq!(generated.media_type, "application/schema+json");
            assert_eq!(generated.text, wrapped(body));
            assert_eq!(schema(&ty), generated);
            assert!(!generated.text.ends_with("\n\n"));
        }
    }

    #[test]
    fn every_primitive_obeys_exact_output_boundaries() {
        for (ty, body) in primitive_vectors() {
            let expected = wrapped(body);
            let mut options = DocumentOptions { max_output_bytes: expected.len() as u32, ..DocumentOptions::default() };
            assert_eq!(type_schema(&ty, &TypeRegistry::default(), &options).unwrap().text, expected);
            options.max_output_bytes -= 1;
            assert_error(type_schema(&ty, &TypeRegistry::default(), &options), ErrorCode::PayloadTooLarge);
        }
    }

    #[test]
    fn fields_are_closed_ordered_described_and_only_outer_optionals_are_omitted() {
        let mut ep = endpoint("project", 1, &[]);
        ep.params = vec![
            field("z", EndpointType::Optional(Box::new(EndpointType::String))),
            field("a", EndpointType::Vec(Box::new(EndpointType::Optional(Box::new(EndpointType::Int64))))),
            field("null", EndpointType::Unit),
        ];
        let expected = wrapped(concat!(
            "\"type\":\"object\",\"properties\":{",
            "\"z\":{\"anyOf\":[{\"type\":\"string\"},{\"type\":\"null\"}],\"description\":\"field\"},",
            "\"a\":{\"type\":\"array\",\"items\":{\"anyOf\":[{\"type\":\"integer\",\"minimum\":-9223372036854775808,\"maximum\":9223372036854775807},{\"type\":\"null\"}]},\"description\":\"field\"},",
            "\"null\":{\"type\":\"null\",\"description\":\"field\"}},",
            "\"required\":[\"a\",\"null\"],\"additionalProperties\":false"
        ));
        let registry = TypeRegistry::default();
        let options = DocumentOptions::default();
        assert_eq!(endpoint_schema(&ep, SchemaDirection::Params, &registry, &options).unwrap().text, expected);
        let response = endpoint_schema(&ep, SchemaDirection::Response, &registry, &options).unwrap();
        assert!(response.text.contains("\"accepted\":{\"type\":\"boolean\""));
        assert!(!response.text.contains("\"z\""));
        ep.params.clear();
        assert_eq!(endpoint_schema(&ep, SchemaDirection::Params, &registry, &options).unwrap().text,
            wrapped(r#""type":"object","properties":{},"required":[],"additionalProperties":false"#));
    }

    #[test]
    fn inline_named_types_live_in_local_definitions() {
        let ty = structure("Pair", vec![field("state", enumeration("State"))]);
        let text = schema(&ty).text;
        assert_eq!(text, wrapped(concat!(
            "\"$ref\":\"#/$defs/Pair\",\"$defs\":{\"Pair\":{",
            "\"type\":\"object\",\"properties\":{\"state\":{\"$ref\":\"#/$defs/State\",\"description\":\"field\"}},",
            "\"required\":[\"state\"],\"additionalProperties\":false},",
            "\"State\":{\"type\":\"integer\",\"enum\":[-2147483648,2147483647],",
            "\"x-enumNames\":[\"Low\",\"High\"],\"x-enumDescriptions\":[\"minimum\",\"maximum\"]}}"
        )));
        let empty = schema(&structure("Empty", Vec::new())).text;
        assert!(empty.contains(r#""Empty":{"type":"object","properties":{},"required":[],"additionalProperties":false}"#));
    }

    #[test]
    fn forward_inline_references_resolve_after_all_fields_are_collected() {
        let ty = structure("Outer", vec![
            field("reference", EndpointType::StructRef(String::from("Inner"))),
            field("declaration", structure("Inner", Vec::new())),
        ]);
        let text = schema(&ty).text;
        assert_eq!(text.matches("\"Inner\":{").count(), 1);
        assert_eq!(text.matches("\"$ref\":\"#/$defs/Inner\"").count(), 2);
    }

    #[test]
    fn recursive_struct_and_table_references_emit_one_definition() {
        let registry = TypeRegistry {
            structs: vec![structure("Node", vec![
                field("next", EndpointType::Optional(Box::new(EndpointType::StructRef(String::from("Node"))))),
            ])], enums: Vec::new(),
        };
        let options = DocumentOptions { max_type_depth: 3, ..DocumentOptions::default() };
        validate_registry(&registry, 3).unwrap();
        let ty = EndpointType::StructTable { struct_ref: String::from("Node") };
        let text = type_schema(&ty, &registry, &options).unwrap().text;
        assert!(text.contains(r##""type":"object","additionalProperties":{"$ref":"#/$defs/Node"}"##));
        assert_eq!(text.matches("\"Node\":{").count(), 1);
        assert!(text.contains("\"required\":[]"));
        assert_error(validate_registry(&registry, 2), ErrorCode::DepthLimitExceeded);
    }

    #[test]
    fn long_mutually_recursive_graph_does_not_consume_inline_depth() {
        let count = 512;
        let mut registry = TypeRegistry::default();
        for index in 0..count {
            registry.structs.push(structure(&format!("N{index}"), vec![
                field("next", EndpointType::StructRef(format!("N{}", (index + 1) % count))),
            ]));
        }
        let options = DocumentOptions { max_type_depth: 2, ..DocumentOptions::default() };
        validate_registry(&registry, 2).unwrap();
        let text = type_schema(&EndpointType::StructRef(String::from("N0")), &registry, &options).unwrap().text;
        assert_eq!(text.matches("\"additionalProperties\":false").count(), count);
        assert_eq!(text.matches("\"$defs\"").count(), 1);
        assert!(text.contains("\"N511\":{\"type\":\"object\""));
    }

    #[test]
    fn enum_references_preserve_values_and_prefix_display_names_only() {
        let registry = TypeRegistry { structs: Vec::new(), enums: vec![enumeration("State")] };
        for prefixed in [false, true] {
            let ty = EndpointType::EnumRef { name: String::from("State"), prefixed_name: prefixed };
            let text = type_schema(&ty, &registry, &DocumentOptions::default()).unwrap().text;
            assert!(text.contains(r#""enum":[-2147483648,2147483647]"#));
            assert!(text.contains(r#""x-enumDescriptions":["minimum","maximum"]"#));
            assert_eq!(text.contains(r#""x-enumNames":["State.Low","State.High"]"#), prefixed);
            assert!(text.contains(r##""$ref":"#/$defs/State""##));
        }
        let empty = EndpointType::Enum { name: String::from("Never"), variants: Vec::new() };
        assert!(schema(&empty).text.contains(r#""enum":[],"x-enumNames":[],"x-enumDescriptions":[]"#));
    }

    #[test]
    fn wrong_registry_lists_and_cross_kind_name_collisions_are_rejected() {
        for registry in [
            TypeRegistry { structs: vec![EndpointType::Unit], enums: Vec::new() },
            TypeRegistry { structs: vec![enumeration("E")], enums: Vec::new() },
            TypeRegistry { structs: Vec::new(), enums: vec![structure("S", Vec::new())] },
        ] {
            assert_error(validate_registry(&registry, 1), ErrorCode::ConstraintViolation);
        }
        for registry in [
            TypeRegistry { structs: vec![structure("Same", Vec::new()), structure("Same", Vec::new())], enums: Vec::new() },
            TypeRegistry { structs: vec![structure("Same", Vec::new())], enums: vec![enumeration("Same")] },
        ] {
            assert_error(validate_registry(&registry, 2), ErrorCode::DuplicateObjectKey);
        }
    }

    #[test]
    fn malformed_definition_variant_and_reference_identifiers_are_rejected() {
        for name in ["", "0bad", "a/b", "a~b", "a.b", "é", "bad name"] {
            assert_error(type_schema(&structure(name, Vec::new()), &TypeRegistry::default(), &DocumentOptions::default()), ErrorCode::ConstraintViolation);
            assert_error(type_schema(&EndpointType::StructRef(String::from(name)), &TypeRegistry::default(), &DocumentOptions::default()), ErrorCode::ConstraintViolation);
            let mut ty = enumeration("State");
            if let EndpointType::Enum { variants, .. } = &mut ty {
                variants[0].name = String::from(name);
            }
            assert_error(type_schema(&ty, &TypeRegistry::default(), &DocumentOptions::default()), ErrorCode::ConstraintViolation);
        }
    }

    #[test]
    fn missing_and_wrong_kind_references_fail_even_in_unused_registry_entries() {
        let registry = TypeRegistry {
            structs: vec![structure("Record", Vec::new())], enums: vec![enumeration("State")],
        };
        for ty in [
            EndpointType::StructRef(String::from("Missing")),
            EndpointType::StructRef(String::from("State")),
            EndpointType::StructTable { struct_ref: String::from("State") },
            EndpointType::EnumRef { name: String::from("Record"), prefixed_name: false },
            EndpointType::EnumRef { name: String::from("Missing"), prefixed_name: true },
        ] {
            assert_error(type_schema(&ty, &registry, &DocumentOptions::default()), ErrorCode::ConstraintViolation);
        }
        let invalid_registry = TypeRegistry {
            structs: vec![structure("Unused", vec![field("missing", EndpointType::StructRef(String::from("Missing")))])],
            enums: Vec::new(),
        };
        assert_error(type_schema(&EndpointType::Unit, &invalid_registry, &DocumentOptions::default()), ErrorCode::ConstraintViolation);
    }

    #[test]
    fn duplicate_fields_variant_names_and_numeric_values_have_duplicate_errors() {
        for fields in [vec![field("", EndpointType::Unit)], vec![field("same", EndpointType::Unit), field("same", EndpointType::String)]] {
            assert_error(type_schema(&structure("Record", fields), &TypeRegistry::default(), &DocumentOptions::default()), ErrorCode::DuplicateObjectKey);
        }
        for same_name in [false, true] {
            let mut ty = enumeration("State");
            if let EndpointType::Enum { variants, .. } = &mut ty {
                if same_name {
                    variants[1].name = variants[0].name.clone();
                } else {
                    variants[1].value = variants[0].value;
                }
            }
            assert_error(type_schema(&ty, &TypeRegistry::default(), &DocumentOptions::default()), ErrorCode::DuplicateObjectKey);
        }
    }

    #[test]
    fn only_exact_registered_inline_copies_are_allowed() {
        let registered = structure("Record", vec![field("value", EndpointType::UInt32)]);
        let registry = TypeRegistry { structs: vec![registered.clone()], enums: vec![enumeration("State")] };
        type_schema(&registered, &registry, &DocumentOptions::default()).unwrap();
        type_schema(&enumeration("State"), &registry, &DocumentOptions::default()).unwrap();
        let mut changed = registered.clone();
        if let EndpointType::Struct { fields, .. } = &mut changed {
            fields[0].description.push('!');
        }
        assert_error(type_schema(&changed, &registry, &DocumentOptions::default()), ErrorCode::DuplicateObjectKey);
        assert_error(type_schema(&structure("State", Vec::new()), &registry, &DocumentOptions::default()), ErrorCode::DuplicateObjectKey);
        let duplicate_inline = structure("Outer", vec![
            field("a", structure("Nested", Vec::new())),
            field("b", structure("Nested", Vec::new())),
        ]);
        assert_error(type_schema(&duplicate_inline, &TypeRegistry::default(), &DocumentOptions::default()), ErrorCode::DuplicateObjectKey);
        let repeated_registered = structure("Outer", vec![field("a", registered.clone()), field("b", registered)]);
        type_schema(&repeated_registered, &registry, &DocumentOptions::default()).unwrap();
    }

    #[test]
    fn registered_copies_reuse_their_nested_declarations_but_recheck_owned_depth() {
        let registered = structure("Record", vec![field("nested", structure("Nested", vec![field("value", EndpointType::Unit)]))]);
        let registry = TypeRegistry { structs: vec![registered.clone()], enums: Vec::new() };
        let options = DocumentOptions { max_type_depth: 3, ..DocumentOptions::default() };
        type_schema(&registered, &registry, &options).unwrap();
        let deeper = EndpointType::Optional(Box::new(registered));
        assert_error(type_schema(&deeper, &registry, &options), ErrorCode::DepthLimitExceeded);
    }

    #[test]
    fn zero_depth_zero_nanoid_and_inline_depth_boundaries_are_checked() {
        let registry = TypeRegistry::default();
        assert_error(validate_registry(&registry, 0), ErrorCode::ConstraintViolation);
        let options = DocumentOptions { max_type_depth: 0, ..DocumentOptions::default() };
        assert_error(type_schema(&EndpointType::Unit, &registry, &options), ErrorCode::ConstraintViolation);
        assert_error(mcp_tools(&[], &registry, Visibility::All, &options), ErrorCode::ConstraintViolation);
        assert_error(type_schema(&EndpointType::NanoId { len: 0 }, &registry, &DocumentOptions::default()), ErrorCode::ConstraintViolation);
        let ty = EndpointType::Optional(Box::new(EndpointType::Vec(Box::new(EndpointType::Unit))));
        let options = DocumentOptions { max_type_depth: 3, ..DocumentOptions::default() };
        type_schema(&ty, &registry, &options).unwrap();
        let options = DocumentOptions { max_type_depth: 2, ..options };
        assert_error(type_schema(&ty, &registry, &options), ErrorCode::DepthLimitExceeded);
        let options = DocumentOptions { max_type_depth: 1, ..options };
        type_schema(&structure("Empty", Vec::new()), &registry, &options).unwrap();
        assert_error(type_schema(&structure("Nonempty", vec![field("v", EndpointType::Unit)]), &registry, &options), ErrorCode::DepthLimitExceeded);
        let mut context = Context::registry(&registry, u16::MAX).unwrap();
        let ty = EndpointType::Optional(Box::new(EndpointType::Unit));
        assert_error(context.visit(Visit::Type(&ty, u32::from(u16::MAX), false)), ErrorCode::DepthLimitExceeded);
    }

    #[test]
    fn deep_owned_types_use_explicit_stacks_for_validation_equality_and_output() {
        fn deep() -> EndpointType {
            let mut ty = EndpointType::Unit;
            for _ in 0..4096 { ty = EndpointType::Optional(Box::new(ty)); }
            ty
        }
        fn dispose(mut ty: EndpointType) {
            while let EndpointType::Optional(child) = ty { ty = *child; }
        }
        let left = deep();
        let right = deep();
        assert!(equal_types(&left, &right).unwrap());
        let options = DocumentOptions { max_type_depth: 4097, ..DocumentOptions::default() };
        let text = type_schema(&left, &TypeRegistry::default(), &options).unwrap().text;
        assert_eq!(text.matches("\"anyOf\"").count(), 4096);
        let options = DocumentOptions { max_type_depth: 4096, ..options };
        assert_error(type_schema(&left, &TypeRegistry::default(), &options), ErrorCode::DepthLimitExceeded);
        dispose(left);
        dispose(right);
    }

    #[test]
    fn exact_visibility_preserves_codes_descriptions_order_and_schemas() {
        let input = catalog();
        let snapshot = input.clone();
        let registry = TypeRegistry::default();
        let options = DocumentOptions::default();
        let all = mcp_tools(&input, &registry, Visibility::All, &options).unwrap();
        assert_eq!(all.iter().map(|tool| tool.name.as_str()).collect::<Vec<_>>(), ["api.read", "api.ping", "api.write"]);
        assert_eq!(all[0].endpoint_code, 0);
        assert_eq!(all[1].endpoint_code, u32::MAX);
        assert_eq!(all[0].description, "é endpoint");
        assert_eq!(all[0].input_schema, endpoint_schema(&input[0].endpoints[0], SchemaDirection::Params, &registry, &options).unwrap());
        assert_eq!(all[0].output_schema, endpoint_schema(&input[0].endpoints[0], SchemaDirection::Response, &registry, &options).unwrap());
        let public = mcp_tools(&input, &registry, Visibility::Public, &options).unwrap();
        assert_eq!(public, all[1..2]);
        let roles = [String::from("reader")];
        assert_eq!(mcp_tools(&input, &registry, Visibility::Roles(&roles), &options).unwrap(), all[..2]);
        let roles = [String::from("Reader"), String::from("read")];
        assert_eq!(mcp_tools(&input, &registry, Visibility::Roles(&roles), &options).unwrap(), public);
        assert_eq!(input, snapshot);
    }

    #[test]
    fn hidden_descriptions_and_unreachable_definitions_are_never_emitted() {
        let mut input = catalog();
        input[0].endpoints[0].description = String::from("PRIVATE_DESCRIPTION");
        input[0].endpoints[0].params[0].ty = structure("PrivateRecord", vec![field("secret", EndpointType::String)]);
        let registry = TypeRegistry {
            structs: vec![structure("UnusedRegistryRecord", vec![field("confidential", EndpointType::String)])],
            enums: vec![enumeration("UnusedRegistryEnum")],
        };
        let public = mcp_tools(&input, &registry, Visibility::Public, &DocumentOptions::default()).unwrap();
        assert_eq!(public.len(), 1);
        for text in [&public[0].input_schema.text, &public[0].output_schema.text, &public[0].description] {
            for secret in ["PRIVATE_DESCRIPTION", "PrivateRecord", "secret", "UnusedRegistryRecord", "confidential", "UnusedRegistryEnum"] {
                assert!(!text.contains(secret));
            }
        }
        assert!(!public[0].input_schema.text.contains("$defs"));
    }

    #[test]
    fn hidden_invalid_types_streams_and_catalog_identities_fail_before_filtering() {
        let options = DocumentOptions::default();
        let registry = TypeRegistry::default();
        for ty in [EndpointType::NanoId { len: 0 }, EndpointType::StructRef(String::from("Missing"))] {
            let mut input = catalog();
            input[0].endpoints[0].stream_response = Some(ty);
            assert_error(mcp_tools(&input, &registry, Visibility::Public, &options), ErrorCode::ConstraintViolation);
        }
        let mut input = catalog();
        input[0].endpoints[0].params = vec![field("repeat", structure("Repeated", Vec::new()))];
        input[0].endpoints[2].response = vec![field("other", structure("Repeated", Vec::new()))];
        assert_error(mcp_tools(&input, &registry, Visibility::Public, &options), ErrorCode::DuplicateObjectKey);
        let mut input = catalog();
        input[0].endpoints[0].service = String::from("wrong");
        assert_error(mcp_tools(&input, &registry, Visibility::Public, &options), ErrorCode::ConstraintViolation);
        let mut input = catalog();
        input[0].endpoints[0].code = u32::MAX;
        assert_error(mcp_tools(&input, &registry, Visibility::Public, &options), ErrorCode::DuplicateObjectKey);
    }

    #[test]
    fn endpoint_validation_includes_unselected_direction_roles_and_streams() {
        let registry = TypeRegistry::default();
        let options = DocumentOptions::default();
        let mut ep = endpoint("project", 0, &[]);
        ep.response[0].ty = EndpointType::StructRef(String::from("Missing"));
        assert_error(endpoint_schema(&ep, SchemaDirection::Params, &registry, &options), ErrorCode::ConstraintViolation);
        let mut ep = endpoint("project", 0, &["admin", "admin"]);
        assert_error(endpoint_schema(&ep, SchemaDirection::Response, &registry, &options), ErrorCode::ConstraintViolation);
        ep.roles = vec![String::new()];
        assert_error(endpoint_schema(&ep, SchemaDirection::Response, &registry, &options), ErrorCode::ConstraintViolation);
        ep.roles.clear();
        ep.stream_response = Some(EndpointType::NanoId { len: 0 });
        assert_error(endpoint_schema(&ep, SchemaDirection::Params, &registry, &options), ErrorCode::ConstraintViolation);
    }

    #[test]
    fn aggregate_tool_budget_counts_both_schemas_and_utf8_metadata_exactly() {
        let input = catalog();
        let registry = TypeRegistry::default();
        let baseline = mcp_tools(&input, &registry, Visibility::All, &DocumentOptions::default()).unwrap();
        let bytes = baseline.iter().map(|tool| {
            tool.name.len() + tool.description.len() + tool.input_schema.text.len() + tool.output_schema.text.len()
        }).sum::<usize>();
        let options = DocumentOptions { max_output_bytes: bytes as u32, ..DocumentOptions::default() };
        assert_eq!(mcp_tools(&input, &registry, Visibility::All, &options).unwrap(), baseline);
        let options = DocumentOptions { max_output_bytes: bytes as u32 - 1, ..options };
        assert_error(mcp_tools(&input, &registry, Visibility::All, &options), ErrorCode::PayloadTooLarge);
        let scalar_count = baseline.iter().map(|tool| {
            tool.name.chars().count() + tool.description.chars().count()
                + tool.input_schema.text.chars().count() + tool.output_schema.text.chars().count()
        }).sum::<usize>();
        let options = DocumentOptions { max_output_bytes: scalar_count as u32, ..options };
        assert_error(mcp_tools(&input, &registry, Visibility::All, &options), ErrorCode::PayloadTooLarge);
    }

    #[test]
    fn zero_output_budget_allows_empty_tool_lists_only() {
        let options = DocumentOptions { max_output_bytes: 0, ..DocumentOptions::default() };
        let registry = TypeRegistry::default();
        assert!(mcp_tools(&[], &registry, Visibility::All, &options).unwrap().is_empty());
        let mut input = catalog();
        input[0].endpoints[1].roles = vec![String::from("private")];
        assert!(mcp_tools(&input, &registry, Visibility::Public, &options).unwrap().is_empty());
        assert_error(mcp_tools(&input, &registry, Visibility::All, &options), ErrorCode::PayloadTooLarge);
        assert_error(type_schema(&EndpointType::Unit, &registry, &options), ErrorCode::PayloadTooLarge);
        assert_error(endpoint_schema(&input[0].endpoints[1], SchemaDirection::Params, &registry, &options), ErrorCode::PayloadTooLarge);
    }

    #[test]
    fn json_strings_escape_all_control_bytes_and_keep_unicode_intact() {
        let mut writer = Writer::new(1024);
        writer.string("\"\\/\x08\x0c\n\r\t\0\x1fé世界🚀").unwrap();
        assert_eq!(writer.text, "\"\\\"\\\\/\\b\\f\\n\\r\\t\\u0000\\u001fé世界🚀\"");
        for byte in 0u8..=31 {
            let raw = [byte];
            let value = core::str::from_utf8(&raw).unwrap();
            let mut writer = Writer::new(8);
            writer.string(value).unwrap();
            let expected = match byte {
                8 => String::from("\"\\b\""), 12 => String::from("\"\\f\""),
                10 => String::from("\"\\n\""), 13 => String::from("\"\\r\""),
                9 => String::from("\"\\t\""), _ => format!("\"\\u{byte:04x}\""),
            };
            assert_eq!(writer.text, expected);
        }
        let mut ep = endpoint("escape", 1, &[]);
        ep.params[0].name = String::from("x\"\\\né");
        ep.params[0].description = String::from("\0\r\t世界");
        let text = endpoint_schema(&ep, SchemaDirection::Params, &TypeRegistry::default(), &DocumentOptions::default()).unwrap().text;
        assert!(text.contains("\"x\\\"\\\\\\né\""));
        assert!(text.contains("\"description\":\"\\u0000\\r\\t世界\""));
        assert_eq!(text.matches("x\\\"\\\\\\né").count(), 2);
    }

    #[test]
    fn json_pointer_components_are_escaped_before_json_quoting() {
        let mut writer = Writer::new(100);
        writer.reference("a~/b\"\\é").unwrap();
        assert_eq!(writer.text, "\"$ref\":\"#/$defs/a~0~1b\\\"\\\\é\"");
    }

    #[test]
    fn writer_and_aggregate_arithmetic_reject_before_appending_or_wrapping() {
        let mut writer = Writer::new(3);
        writer.raw("é").unwrap();
        let capacity = writer.text.capacity();
        assert_error(writer.raw("xx"), ErrorCode::PayloadTooLarge);
        assert_eq!(writer.text, "é");
        assert_eq!(writer.text.capacity(), capacity);
        writer.raw("!").unwrap();
        writer.raw("").unwrap();
        assert_error(writer.raw("x"), ErrorCode::PayloadTooLarge);
        let mut remaining = 0;
        assert_error(charge(&mut remaining, usize::MAX), ErrorCode::PayloadTooLarge);
        assert_eq!(remaining, 0);
        let mut values: Vec<u8> = Vec::new();
        assert!(values.try_reserve(usize::MAX).is_err());
        let error = allocation_error();
        assert_eq!(error.code, ErrorCode::BudgetExhausted);
        assert_eq!(error.source, ErrorSource::Encode);
        assert_eq!(copy_text("é").unwrap(), "é");
    }

    #[test]
    fn named_documents_and_unicode_descriptions_obey_exact_byte_limits() {
        let mut ep = endpoint("named", 3, &[]);
        ep.params[0].ty = structure("Record", vec![field("state", enumeration("State"))]);
        ep.params[0].description = String::from("é世界\n\0");
        let registry = TypeRegistry::default();
        let expected = endpoint_schema(&ep, SchemaDirection::Params, &registry, &DocumentOptions::default()).unwrap();
        let options = DocumentOptions { max_output_bytes: expected.text.len() as u32, ..DocumentOptions::default() };
        assert_eq!(endpoint_schema(&ep, SchemaDirection::Params, &registry, &options).unwrap(), expected);
        let options = DocumentOptions { max_output_bytes: expected.text.len() as u32 - 1, ..options };
        assert_error(endpoint_schema(&ep, SchemaDirection::Params, &registry, &options), ErrorCode::PayloadTooLarge);
        let options = DocumentOptions { max_output_bytes: expected.text.chars().count() as u32, ..options };
        assert_error(endpoint_schema(&ep, SchemaDirection::Params, &registry, &options), ErrorCode::PayloadTooLarge);
    }

    #[test]
    fn valid_stream_schemas_are_separate_from_immediate_tool_outputs() {
        let mut input = catalog();
        input[0].endpoints[1].stream_response = Some(structure("StreamResult", vec![field("stream_only", EndpointType::Int64)]));
        let tools = mcp_tools(&input, &TypeRegistry::default(), Visibility::Public, &DocumentOptions::default()).unwrap();
        assert_eq!(tools.len(), 1);
        for text in [&tools[0].input_schema.text, &tools[0].output_schema.text] {
            assert!(!text.contains("StreamResult"));
            assert!(!text.contains("stream_only"));
        }
        assert!(tools[0].output_schema.text.contains("accepted"));
    }

    #[test]
    fn iterative_equality_compares_all_named_type_parameters_and_field_metadata() {
        for (left, right) in [
            (EndpointType::UInt32, EndpointType::Int32),
            (EndpointType::NanoId { len: 1 }, EndpointType::NanoId { len: 2 }),
            (EndpointType::StructRef(String::from("A")), EndpointType::StructRef(String::from("B"))),
            (EndpointType::StructTable { struct_ref: String::from("A") }, EndpointType::StructTable { struct_ref: String::from("B") }),
            (EndpointType::EnumRef { name: String::from("E"), prefixed_name: false }, EndpointType::EnumRef { name: String::from("E"), prefixed_name: true }),
            (structure("A", Vec::new()), structure("B", Vec::new())),
            (structure("A", Vec::new()), structure("A", vec![field("x", EndpointType::Unit)])),
            (structure("A", vec![field("x", EndpointType::Unit)]), structure("A", vec![field("y", EndpointType::Unit)])),
            (enumeration("A"), enumeration("B")),
        ] {
            assert!(!equal_types(&left, &right).unwrap());
            assert!(equal_types(&left, &left.clone()).unwrap());
        }
        let left = enumeration("E");
        let mut right = left.clone();
        if let EndpointType::Enum { variants, .. } = &mut right {
            variants[0].description.push('!');
        }
        assert!(!equal_types(&left, &right).unwrap());
    }

    #[test]
    fn integer_writer_is_exact_at_every_signed_and_unsigned_boundary() {
        for (number, expected) in [
            (i64::MIN, "-9223372036854775808"), (i64::MAX, "9223372036854775807"),
            (i64::from(i32::MIN), "-2147483648"), (i64::from(i32::MAX), "2147483647"),
            (i64::from(u32::MAX), "4294967295"), (-1, "-1"), (0, "0"), (1, "1"),
        ] {
            let mut writer = Writer::new(expected.len() as u32);
            writer.integer(number).unwrap();
            assert_eq!(writer.text, expected);
        }
    }
}
