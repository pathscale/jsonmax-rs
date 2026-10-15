//! Deterministic, bounded Markdown documentation and systemd unit generation.
//!
//! All fallible projections validate metadata before writing. Output limits count
//! UTF-8 bytes, including escaping and the single final newline. Owned type trees
//! are walked with fallible explicit stacks; named references are never expanded.

use super::*;

/// Documents visible services in input order after validating the entire catalog.
///
/// Only an outer `Optional` makes a field optional. Named references retain their
/// names, so recursive models remain finite. `server_url` is not rendered here.
pub fn service_docs(
    services: &[ServiceSpec],
    registry: &TypeRegistry,
    visibility: Visibility<'_>,
    options: &DocumentOptions,
) -> Result<GeneratedDocument> {
    validate_services(services)?;
    let mut context = Context::registry(registry, options.max_type_depth)?;
    for service in services {
        for endpoint in &service.endpoints {
            context.endpoint(endpoint)?;
        }
    }
    context.resolve()?;

    let mut out = Writer::new(options.max_output_bytes);
    out.raw("# ")?;
    out.markdown(&options.title)?;
    out.raw("\n\nVersion: ")?;
    out.markdown(&options.version)?;
    out.raw("\n")?;
    if !options.description.is_empty() {
        out.raw("\n")?;
        out.markdown(&options.description)?;
        out.raw("\n")?;
    }
    for service in services {
        if !service.endpoints.iter().any(|endpoint| visibility.includes(endpoint)) {
            continue;
        }
        out.raw("\n## Service: ")?;
        out.markdown(&service.name)?;
        out.raw("\n\nService ID: ")?;
        out.integer(i64::from(service.id))?;
        out.raw("\n")?;
        for endpoint in &service.endpoints {
            if !visibility.includes(endpoint) {
                continue;
            }
            out.raw("\n### ")?;
            out.markdown(&service.name)?;
            out.raw(".")?;
            out.markdown(&endpoint.name)?;
            out.raw("\n\nCode: ")?;
            out.integer(i64::from(endpoint.code))?;
            out.raw("\n\nRoles: ")?;
            if endpoint.roles.is_empty() {
                out.raw("Public")?;
            } else {
                for (index, role) in endpoint.roles.iter().enumerate() {
                    if index != 0 { out.raw(", ")?; }
                    out.markdown(role)?;
                }
            }
            out.raw("\n")?;
            if !endpoint.description.is_empty() {
                out.raw("\n")?;
                out.markdown(&endpoint.description)?;
                out.raw("\n")?;
            }
            write_fields(&mut out, "Parameters", &endpoint.params)?;
            write_fields(&mut out, "Response", &endpoint.response)?;
            out.raw("\n#### Streaming response\n\n")?;
            if let Some(ty) = &endpoint.stream_response {
                write_type(&mut out, ty)?;
            } else {
                out.raw("None")?;
            }
            out.raw("\n")?;
        }
    }
    GeneratedDocument::new("text/markdown", out.text, options.max_output_bytes)
}

/// Emits an ordered error table, rejecting ambiguous codes or identifiers.
pub fn error_docs(errors: &[ErrorDoc], max_output_bytes: u32) -> Result<GeneratedDocument> {
    for (index, error) in errors.iter().enumerate() {
        if !valid_identifier(&error.name) {
            return Err(invalid("invalid error documentation identifier"));
        }
        if errors[..index].iter().any(|previous| {
            previous.code == error.code || previous.name == error.name
        }) {
            return Err(duplicate("duplicate error documentation code or name"));
        }
    }
    let mut out = Writer::new(max_output_bytes);
    out.raw("# Errors\n\n| Code | Name | Description |\n| --- | --- | --- |\n")?;
    for error in errors {
        out.raw("| ")?;
        out.integer(i64::from(error.code))?;
        out.raw(" | ")?;
        out.markdown(&error.name)?;
        out.raw(" | ")?;
        out.markdown(&error.description)?;
        out.raw(" |\n")?;
    }
    GeneratedDocument::new("text/markdown", out.text, max_output_bytes)
}

/// Emits a simple service unit without touching the filesystem or processes.
///
/// Each ExecStart word is independently double quoted and protects literal
/// percent, dollar, backslash, and double quote characters. Scalar directives
/// use their own syntax: they expand percent specifiers but do not unquote
/// ExecStart words or expand environment variables.
pub fn systemd_unit(options: &SystemdOptions, max_output_bytes: u32) -> Result<GeneratedDocument> {
    validate_systemd(options)?;
    let mut out = Writer::new(max_output_bytes);
    out.raw("[Unit]\nDescription=")?;
    out.directive(&options.description)?;
    out.raw("\nAfter=network-online.target\n\n[Service]\nType=simple\nExecStart=")?;
    out.exec_word(&options.executable)?;
    for argument in &options.arguments {
        out.raw(" ")?;
        out.exec_word(argument)?;
    }
    out.raw("\n")?;
    if let Some(user) = &options.user {
        out.raw("User=")?;
        out.raw(user)?;
        out.raw("\n")?;
    }
    if let Some(directory) = &options.working_directory {
        out.raw("WorkingDirectory=")?;
        // This directive is one unquoted path, not an ExecStart word. A final
        // space would be stripped by the unit parser. Appending /. addresses
        // the same directory while preserving a space in its final component.
        if directory.ends_with(' ') {
            out.specifiers(directory)?;
            out.raw("/.")?;
        } else {
            out.directive(directory)?;
        }
        out.raw("\n")?;
    }
    out.raw("Restart=on-failure\nRestartSec=")?;
    out.integer(i64::from(options.restart_seconds))?;
    out.raw("\n\n[Install]\nWantedBy=multi-user.target\n")?;
    GeneratedDocument::new("text/plain", out.text, max_output_bytes)
}

/// Escapes all ASCII punctuation, encodes HTML-sensitive characters, flattens
/// CR/LF, and preserves every other Unicode scalar value.
///
/// This infallible convenience API has the usual allocation behavior of String.
/// The fallible document APIs instead use the same escaping rules directly in
/// their bounded writer and report allocation failure as `BudgetExhausted`.
pub fn escape_markdown(text: &str) -> String {
    let mut escaped = String::new();
    for ch in text.chars() {
        if let Some(replacement) = markdown_replacement(ch) {
            escaped.push_str(replacement);
        } else {
            if ch.is_ascii_punctuation() { escaped.push('\\'); }
            escaped.push(ch);
        }
    }
    escaped
}

fn markdown_replacement(ch: char) -> Option<&'static str> {
    match ch {
        '\r' | '\n' => Some(" "),
        '&' => Some("&amp;"),
        '<' => Some("&lt;"),
        '>' => Some("&gt;"),
        _ => None,
    }
}

fn validate_systemd(options: &SystemdOptions) -> Result<()> {
    for value in core::iter::once(options.description.as_str())
        .chain(core::iter::once(options.executable.as_str()))
        .chain(options.arguments.iter().map(String::as_str))
        .chain(options.user.as_deref())
        .chain(options.working_directory.as_deref())
    {
        if value.chars().any(char::is_control) {
            return Err(invalid("control character in systemd option"));
        }
    }
    if !options.executable.starts_with('/') {
        return Err(invalid("systemd executable must be an absolute path"));
    }
    if let Some(directory) = &options.working_directory {
        if !directory.starts_with('/') {
            return Err(invalid("systemd working directory must be an absolute path"));
        }
    }
    if let Some(user) = &options.user {
        if user.is_empty() || !user.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')
        }) {
            return Err(invalid("invalid systemd user identifier"));
        }
    }
    Ok(())
}

fn allocation_error() -> Error {
    spec_error(ErrorCode::BudgetExhausted, "documentation allocation failed")
}

fn output_error() -> Error {
    spec_error(ErrorCode::PayloadTooLarge, "documentation exceeds output byte limit")
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

    fn character(&mut self, ch: char) -> Result<()> {
        let mut bytes = [0; 4];
        self.raw(ch.encode_utf8(&mut bytes))
    }

    fn markdown(&mut self, text: &str) -> Result<()> {
        for ch in text.chars() {
            if let Some(replacement) = markdown_replacement(ch) {
                self.raw(replacement)?;
            } else {
                if ch.is_ascii_punctuation() { self.raw("\\")?; }
                self.character(ch)?;
            }
        }
        Ok(())
    }

    fn integer(&mut self, value: i64) -> Result<()> {
        let mut bytes = [0u8; 20];
        let mut position = bytes.len();
        let mut magnitude = value.unsigned_abs();
        loop {
            position -= 1;
            bytes[position] = b'0' + (magnitude % 10) as u8;
            magnitude /= 10;
            if magnitude == 0 { break; }
        }
        if value < 0 {
            position -= 1;
            bytes[position] = b'-';
        }
        // Every byte is an ASCII digit or the one leading minus sign.
        let text = core::str::from_utf8(&bytes[position..])
            .map_err(|_| spec_error(ErrorCode::InternalError, "invalid integer text"))?;
        self.raw(text)
    }

    fn exec_word(&mut self, text: &str) -> Result<()> {
        self.raw("\"")?;
        for ch in text.chars() {
            match ch {
                '%' => self.raw("%%")?,
                '$' => self.raw("$$")?,
                '\\' => self.raw("\\\\")?,
                '"' => self.raw("\\\"")?,
                _ => self.character(ch)?,
            }
        }
        self.raw("\"")
    }

    fn specifiers(&mut self, text: &str) -> Result<()> {
        for ch in text.chars() {
            if ch == '%' { self.raw("%%")?; } else { self.character(ch)?; }
        }
        Ok(())
    }

    fn directive(&mut self, text: &str) -> Result<()> {
        self.specifiers(text)?;
        // The generic unit parser detects continuation before stripping value
        // whitespace. A protective space prevents a final backslash from
        // swallowing the following directive and is then stripped normally.
        // See systemd src/shared/conf-parser.c and config_parse_working_directory
        // in src/core/load-fragment.c. Scalar values are not C-unescaped.
        if text.ends_with('\\') { self.raw(" ")?; }
        Ok(())
    }
}

fn write_fields(out: &mut Writer, heading: &str, fields: &[EndpointField]) -> Result<()> {
    out.raw("\n#### ")?;
    out.raw(heading)?;
    out.raw("\n\n| Name | Type | Optional | Description |\n| --- | --- | --- | --- |\n")?;
    for field in fields {
        out.raw("| ")?;
        out.markdown(&field.name)?;
        out.raw(" | ")?;
        write_type(out, &field.ty)?;
        out.raw(if matches!(&field.ty, EndpointType::Optional(_)) { " | Yes | " } else { " | No | " })?;
        out.markdown(&field.description)?;
        out.raw(" |\n")?;
    }
    Ok(())
}

enum Render<'a> {
    Type(&'a EndpointType),
    Text(&'a str),
    Fields(&'a [EndpointField], bool),
    Variants(&'a [EnumVariantSpec], bool),
}

fn write_type(out: &mut Writer, ty: &EndpointType) -> Result<()> {
    let mut pending = Vec::new();
    push(&mut pending, Render::Type(ty))?;
    while let Some(task) = pending.pop() {
        match task {
            Render::Text(text) => out.markdown(text)?,
            Render::Fields(fields, first) => {
                if let Some((field, rest)) = fields.split_first() {
                    if !first { out.markdown(", ")?; }
                    out.markdown(&field.name)?;
                    out.markdown(": ")?;
                    push(&mut pending, Render::Fields(rest, false))?;
                    if !field.description.is_empty() {
                        push(&mut pending, Render::Text(")"))?;
                        push(&mut pending, Render::Text(&field.description))?;
                        push(&mut pending, Render::Text(" ("))?;
                    }
                    push(&mut pending, Render::Type(&field.ty))?;
                }
            }
            Render::Variants(variants, first) => {
                if let Some((variant, rest)) = variants.split_first() {
                    if !first { out.markdown(", ")?; }
                    out.markdown(&variant.name)?;
                    out.markdown(" = ")?;
                    out.integer(i64::from(variant.value))?;
                    if !variant.description.is_empty() {
                        out.markdown(" (")?;
                        out.markdown(&variant.description)?;
                        out.markdown(")")?;
                    }
                    push(&mut pending, Render::Variants(rest, false))?;
                }
            }
            Render::Type(ty) => match ty {
                EndpointType::UInt32 => out.markdown("UInt32")?,
                EndpointType::Int32 => out.markdown("Int32")?,
                EndpointType::Int64 => out.markdown("Int64")?,
                EndpointType::Float64 => out.markdown("Float64")?,
                EndpointType::Boolean => out.markdown("Boolean")?,
                EndpointType::String => out.markdown("String")?,
                EndpointType::Bytea => out.markdown("Bytea")?,
                EndpointType::UUID => out.markdown("UUID")?,
                EndpointType::NanoId { len } => {
                    out.markdown("NanoId(")?;
                    out.integer(i64::from(*len))?;
                    out.markdown(")")?;
                }
                EndpointType::IpAddr => out.markdown("IpAddr")?,
                EndpointType::Struct { name, fields } => {
                    out.markdown("Struct ")?;
                    out.markdown(name)?;
                    out.markdown(" { ")?;
                    push(&mut pending, Render::Text(" }"))?;
                    push(&mut pending, Render::Fields(fields, true))?;
                }
                EndpointType::StructRef(name) => {
                    out.markdown("StructRef ")?;
                    out.markdown(name)?;
                }
                EndpointType::Object => out.markdown("Object")?,
                EndpointType::StructTable { struct_ref } => {
                    out.markdown("StructTable<")?;
                    out.markdown(struct_ref)?;
                    out.markdown(">")?;
                }
                EndpointType::Vec(child) | EndpointType::Optional(child) => {
                    out.markdown(if matches!(ty, EndpointType::Vec(_)) { "Vec<" } else { "Optional<" })?;
                    push(&mut pending, Render::Text(">"))?;
                    push(&mut pending, Render::Type(child))?;
                }
                EndpointType::Unit => out.markdown("Unit")?,
                EndpointType::Enum { name, variants } => {
                    out.markdown("Enum ")?;
                    out.markdown(name)?;
                    out.markdown(" { ")?;
                    push(&mut pending, Render::Text(" }"))?;
                    push(&mut pending, Render::Variants(variants, true))?;
                }
                EndpointType::EnumRef { name, prefixed_name } => {
                    out.markdown("EnumRef ")?;
                    out.markdown(name)?;
                    if *prefixed_name { out.markdown(" (prefixed names)")?; }
                }
                EndpointType::TimeStampMs => out.markdown("TimeStampMs (milliseconds)")?,
                EndpointType::BlockchainDecimal => out.markdown("BlockchainDecimal")?,
                EndpointType::BlockchainAddress => out.markdown("BlockchainAddress")?,
                EndpointType::BlockchainTransactionHash => out.markdown("BlockchainTransactionHash")?,
            },
        }
    }
    Ok(())
}

// Local validation follows the frozen schema semantics. Keeping this
// borrowed index local avoids coupling to another wave's private helpers
// or allocating JSON schemas merely to validate Markdown input.
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

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{boxed::Box, vec};

    fn field(name: &str, ty: EndpointType) -> EndpointField {
        EndpointField { name: String::from(name), ty, description: String::new() }
    }

    fn endpoint(name: &str, code: u32, roles: &[&str]) -> EndpointSchemaSpec {
        EndpointSchemaSpec {
            code,
            name: String::from(name),
            service: String::from("api"),
            roles: roles.iter().map(|role| String::from(*role)).collect(),
            params: Vec::new(),
            response: Vec::new(),
            stream_response: None,
            description: String::new(),
        }
    }

    fn catalog() -> Vec<ServiceSpec> {
        vec![ServiceSpec {
            id: 42,
            name: String::from("api"),
            endpoints: vec![
                endpoint("ping", 0, &[]),
                endpoint("watch", 7, &["reader", "admin"]),
                endpoint("hidden", u32::MAX, &["owner"]),
            ],
        }]
    }

    fn options() -> DocumentOptions {
        DocumentOptions {
            title: String::from("API"), version: String::from("v1"),
            description: String::new(), ..DocumentOptions::default()
        }
    }

    fn unit_options() -> SystemdOptions {
        SystemdOptions {
            description: String::from("API server"),
            executable: String::from("/usr/bin/jmax"),
            arguments: Vec::new(), user: None, working_directory: None,
            restart_seconds: 5,
        }
    }

    fn structure(name: &str, fields: Vec<EndpointField>) -> EndpointType {
        EndpointType::Struct { name: String::from(name), fields }
    }

    fn enumeration(name: &str, variants: &[(&str, i32)]) -> EndpointType {
        EndpointType::Enum {
            name: String::from(name),
            variants: variants.iter().map(|(name, value)| EnumVariantSpec {
                name: String::from(*name), value: *value, description: String::new(),
            }).collect(),
        }
    }

    fn assert_error<T>(result: Result<T>, code: ErrorCode) {
        match result {
            Ok(_) => panic!("expected an error"),
            Err(error) => {
                assert_eq!(error.code, code);
                assert_eq!(error.source, ErrorSource::Encode);
                assert_eq!(error.byte_offset, 0);
                assert_eq!(error.schema_id, 0);
                assert_eq!(error.field_id, 0);
                assert!(!error.detail.is_empty());
            }
        }
    }

    fn assert_document(document: &GeneratedDocument, media_type: &str) {
        assert_eq!(document.media_type, media_type);
        assert!(document.text.ends_with('\n'));
        assert!(!document.text.ends_with("\n\n"));
        assert!(!document.text.contains('\r'));
    }

    fn rendered(ty: &EndpointType) -> String {
        let mut out = Writer::new(u32::MAX);
        write_type(&mut out, ty).unwrap();
        out.text
    }

    #[test]
    fn markdown_escape_has_exact_injection_and_unicode_vectors() {
        assert_eq!(escape_markdown(""), "");
        assert_eq!(escape_markdown("plain é雪😀"), "plain é雪😀");
        assert_eq!(escape_markdown("a\r\nb\rc\nd"), "a  b c d");
        assert_eq!(
            escape_markdown("<a href=\"x&y\">[x](u)|`\\\r\né雪😀"),
            r#"&lt;a href\=\"x&amp;y\"&gt;\[x\]\(u\)\|\`\\  é雪😀"#,
        );
        assert_eq!(escape_markdown("&#60;script>"), r"&amp;\#60\;script&gt;");
        assert_eq!(escape_markdown("![link](javascript:x)"), r"\!\[link\]\(javascript\:x\)");
        assert_eq!(escape_markdown("\t\0\u{7f}\u{2028}"), "\t\0\u{7f}\u{2028}");
    }

    #[test]
    fn every_ascii_character_obeys_the_same_public_and_bounded_escape_rules() {
        for byte in 0u8..=127 {
            let ch = char::from(byte);
            let text: String = core::iter::once(ch).collect();
            let expected = match ch {
                '&' => String::from("&amp;"),
                '<' => String::from("&lt;"),
                '>' => String::from("&gt;"),
                '\r' | '\n' => String::from(" "),
                _ => {
                    let mut value = String::new();
                    if matches!(byte, 33..=47 | 58..=64 | 91..=96 | 123..=126) {
                        value.push('\\');
                    }
                    value.push(ch);
                    value
                }
            };
            assert_eq!(escape_markdown(&text), expected);
            let mut out = Writer::new(expected.len() as u32);
            out.markdown(&text).unwrap();
            assert_eq!(out.text, expected);
            let mut limited = Writer::new(expected.len() as u32 - 1);
            assert_error(limited.markdown(&text), ErrorCode::PayloadTooLarge);
            assert!(limited.text.len() < expected.len());
        }
    }

    #[test]
    fn empty_catalog_and_empty_error_list_have_exact_nonempty_documents() {
        let document = service_docs(&[], &TypeRegistry::default(), Visibility::All, &options()).unwrap();
        assert_eq!(document.text, "# API\n\nVersion: v1\n");
        assert_document(&document, "text/markdown");
        let document = error_docs(&[], u32::MAX).unwrap();
        assert_eq!(document.text, "# Errors\n\n| Code | Name | Description |\n| --- | --- | --- |\n");
        assert_document(&document, "text/markdown");
        let document = service_docs(&[], &TypeRegistry::default(), Visibility::All, &DocumentOptions::default()).unwrap();
        assert_eq!(document.text, "# Endpoint API\n\nVersion: 1\\.0\\.0\n");
    }

    #[test]
    fn complete_service_document_matches_the_frozen_type_and_optionality_vector() {
        let mut input = catalog();
        input[0].endpoints.truncate(1);
        input[0].endpoints[0].description = String::from("A <ping>.");
        input[0].endpoints[0].params = vec![EndpointField {
            name: String::from("a|b"),
            ty: EndpointType::Optional(Box::new(EndpointType::String)),
            description: String::from("Use [x].\r\nNext"),
        }];
        input[0].endpoints[0].response = vec![field("ok", EndpointType::Boolean)];
        input[0].endpoints[0].stream_response = Some(EndpointType::Vec(Box::new(EndpointType::Int64)));
        let mut opts = options();
        opts.description = String::from("Document & overview");
        let document = service_docs(&input, &TypeRegistry::default(), Visibility::All, &opts).unwrap();
        assert_eq!(document.text, concat!(
            "# API\n\nVersion: v1\n\nDocument &amp; overview\n",
            "\n## Service: api\n\nService ID: 42\n",
            "\n### api.ping\n\nCode: 0\n\nRoles: Public\n\nA &lt;ping&gt;\\.\n",
            "\n#### Parameters\n\n| Name | Type | Optional | Description |\n| --- | --- | --- | --- |\n",
            "| a\\|b | Optional&lt;String&gt; | Yes | Use \\[x\\]\\.  Next |\n",
            "\n#### Response\n\n| Name | Type | Optional | Description |\n| --- | --- | --- | --- |\n",
            "| ok | Boolean | No |  |\n",
            "\n#### Streaming response\n\nVec&lt;Int64&gt;\n",
        ));
        assert_document(&document, "text/markdown");
    }

    #[test]
    fn visibility_matches_service_selection_and_mcp_tools_without_reordering_or_mutation() {
        let mut input = catalog();
        input[0].endpoints[1].stream_response = Some(EndpointType::String);
        input.push(ServiceSpec { id: 43, name: String::from("empty"), endpoints: Vec::new() });
        input.push(ServiceSpec {
            id: 44, name: String::from("secret"), endpoints: vec![EndpointSchemaSpec {
                service: String::from("secret"), ..endpoint("last", 9, &["owner"])
            }],
        });
        let snapshot = input.clone();
        let roles = vec![String::from("reader")];
        let unmatched = vec![String::from("Reader"), String::from("read")];
        for (visibility, expected) in [
            (Visibility::Public, vec!["api.ping"]),
            (Visibility::Roles(&roles), vec!["api.ping", "api.watch"]),
            (Visibility::Roles(&unmatched), vec!["api.ping"]),
            (Visibility::Roles(&[]), vec!["api.ping"]),
            (Visibility::All, vec!["api.ping", "api.watch", "api.hidden", "secret.last"]),
        ] {
            let opts = options();
            let registry = TypeRegistry::default();
            let document = service_docs(&input, &registry, visibility, &opts).unwrap();
            let names: Vec<&str> = document.text.lines().filter_map(|line| line.strip_prefix("### ")).collect();
            assert_eq!(names, expected);
            let selected = services::select_services(&input, visibility).unwrap();
            let selected_names: Vec<String> = selected.iter().flat_map(|service| {
                service.endpoints.iter().map(move |endpoint| tool_name(&service.name, &endpoint.name).unwrap())
            }).collect();
            let tools = json_schema::mcp_tools(&input, &registry, visibility, &opts).unwrap();
            assert_eq!(selected_names.iter().map(String::as_str).collect::<Vec<_>>(), expected);
            assert_eq!(tools.iter().map(|tool| tool.name.as_str()).collect::<Vec<_>>(), expected);
            assert!(!document.text.contains("## Service: empty"));
            if !matches!(visibility, Visibility::All) {
                assert!(!document.text.contains("hidden"));
                assert!(!document.text.contains("secret"));
                assert!(!document.text.contains("owner"));
            }
            for _ in 0..3 {
                assert_eq!(service_docs(&input, &registry, visibility, &opts).unwrap(), document);
            }
        }
        assert_eq!(input, snapshot);
    }

    #[test]
    fn headings_descriptions_roles_and_nested_metadata_cannot_add_markdown_structure() {
        let attack = "</table>\r\n# Heading | [link](url) `code` \\ &";
        let mut input = catalog();
        let mut nested = field(attack, EndpointType::String);
        nested.description = String::from(attack);
        input[0].endpoints[0].params = vec![EndpointField {
            name: String::from(attack),
            ty: structure("Record", vec![nested]),
            description: String::from(attack),
        }];
        input[0].endpoints[0].description = String::from(attack);
        input[0].endpoints[0].roles = vec![String::from(attack)];
        let mut opts = options();
        opts.title = String::from(attack);
        opts.version = String::from(attack);
        opts.description = String::from(attack);
        let document = service_docs(&input, &TypeRegistry::default(), Visibility::All, &opts).unwrap();
        let escaped = escape_markdown(attack);
        assert!(document.text.starts_with(&alloc::format!("# {escaped}\n")));
        assert_eq!(document.text.matches(escaped.as_str()).count(), 9);
        assert!(!document.text.contains('<'));
        assert!(!document.text.contains('>'));
        assert!(!document.text.contains("\n# Heading"));
        assert!(!document.text.contains("[link](url)"));
        assert!(!document.text.contains("`code`"));
        assert!(!document.text.contains('\r'));
    }

    #[test]
    fn every_endpoint_type_has_an_exact_finite_display_vector() {
        let vectors = vec![
            (EndpointType::UInt32, "UInt32"),
            (EndpointType::Int32, "Int32"),
            (EndpointType::Int64, "Int64"),
            (EndpointType::Float64, "Float64"),
            (EndpointType::Boolean, "Boolean"),
            (EndpointType::String, "String"),
            (EndpointType::Bytea, "Bytea"),
            (EndpointType::UUID, "UUID"),
            (EndpointType::NanoId { len: 1 }, r"NanoId\(1\)"),
            (EndpointType::NanoId { len: u8::MAX }, r"NanoId\(255\)"),
            (EndpointType::IpAddr, "IpAddr"),
            (structure("Empty", Vec::new()), r"Struct Empty \{  \}"),
            (EndpointType::StructRef(String::from("Node")), "StructRef Node"),
            (EndpointType::Object, "Object"),
            (EndpointType::StructTable { struct_ref: String::from("Node") }, "StructTable&lt;Node&gt;"),
            (EndpointType::Vec(Box::new(EndpointType::Unit)), "Vec&lt;Unit&gt;"),
            (EndpointType::Unit, "Unit"),
            (EndpointType::Optional(Box::new(EndpointType::Unit)), "Optional&lt;Unit&gt;"),
            (enumeration("State", &[]), r"Enum State \{  \}"),
            (EndpointType::EnumRef { name: String::from("State"), prefixed_name: false }, "EnumRef State"),
            (EndpointType::EnumRef { name: String::from("State"), prefixed_name: true }, r"EnumRef State \(prefixed names\)"),
            (EndpointType::TimeStampMs, r"TimeStampMs \(milliseconds\)"),
            (EndpointType::BlockchainDecimal, "BlockchainDecimal"),
            (EndpointType::BlockchainAddress, "BlockchainAddress"),
            (EndpointType::BlockchainTransactionHash, "BlockchainTransactionHash"),
        ];
        let registry = TypeRegistry {
            structs: vec![structure("Node", Vec::new())],
            enums: vec![enumeration("State", &[])],
        };
        let mut input = catalog();
        input[0].endpoints.truncate(1);
        for (index, (ty, expected)) in vectors.iter().enumerate() {
            assert_eq!(rendered(ty), *expected);
            input[0].endpoints[0].params = vec![field("value", ty.clone())];
            let document = service_docs(&input, &registry, Visibility::All, &options()).unwrap();
            assert!(document.text.contains(*expected), "type vector {index}");
            assert!(json_schema::type_schema(ty, &registry, &options()).is_ok());
        }
        let enum_type = EndpointType::Enum {
            name: String::from("Bounds"), variants: vec![
                EnumVariantSpec { name: String::from("Min"), value: i32::MIN, description: String::from("<zero>|\n") },
                EnumVariantSpec { name: String::from("Max"), value: i32::MAX, description: String::new() },
            ],
        };
        assert_eq!(rendered(&enum_type), r"Enum Bounds \{ Min \= -2147483648 \(&lt;zero&gt;\| \)\, Max \= 2147483647 \}");
        let ty = structure("Outer", vec![field("items", EndpointType::Vec(Box::new(EndpointType::Optional(Box::new(EndpointType::Int32))))) ]);
        assert_eq!(rendered(&ty), r"Struct Outer \{ items\: Vec&lt;Optional&lt;Int32&gt;&gt; \}");
    }

    #[test]
    fn only_outer_optional_fields_are_marked_optional_and_all_orders_are_preserved() {
        let mut input = catalog();
        input[0].endpoints.truncate(1);
        input[0].endpoints[0].params = vec![
            field("z", EndpointType::Optional(Box::new(EndpointType::Vec(Box::new(EndpointType::String))))),
            field("a", EndpointType::Vec(Box::new(EndpointType::Optional(Box::new(EndpointType::String))))),
            field("null", EndpointType::Unit),
        ];
        input[0].endpoints[0].roles = vec![String::from("z-role"), String::from("a-role")];
        let document = service_docs(&input, &TypeRegistry::default(), Visibility::All, &options()).unwrap();
        assert!(document.text.contains("Roles: z\\-role, a\\-role\n"));
        assert!(document.text.contains("| z | Optional&lt;Vec&lt;String&gt;&gt; | Yes |"));
        assert!(document.text.contains("| a | Vec&lt;Optional&lt;String&gt;&gt; | No |"));
        assert!(document.text.contains("| null | Unit | No |"));
        assert!(document.text.find("| z |").unwrap() < document.text.find("| a |").unwrap());
        assert!(document.text.find("| a |").unwrap() < document.text.find("| null |").unwrap());
    }

    #[test]
    fn invalid_hidden_catalogs_fail_before_filtering_or_output_limits() {
        let mut input = catalog();
        let opts = DocumentOptions { max_output_bytes: 0, ..options() };
        let registry = TypeRegistry::default();
        input[0].endpoints[2].params = vec![field("bad", EndpointType::NanoId { len: 0 })];
        assert_error(service_docs(&input, &registry, Visibility::Public, &opts), ErrorCode::ConstraintViolation);
        input[0].endpoints[2].params[0].ty = EndpointType::StructRef(String::from("Missing"));
        assert_error(service_docs(&input, &registry, Visibility::Public, &opts), ErrorCode::ConstraintViolation);
        input[0].endpoints[2].params.clear();
        input[0].endpoints[2].stream_response = Some(EndpointType::EnumRef { name: String::from("Missing"), prefixed_name: true });
        assert_error(service_docs(&input, &registry, Visibility::Public, &opts), ErrorCode::ConstraintViolation);
        input[0].endpoints[2].stream_response = None;
        input[0].endpoints[2].response = vec![field("x", EndpointType::String), field("x", EndpointType::Unit)];
        assert_error(service_docs(&input, &registry, Visibility::Public, &opts), ErrorCode::DuplicateObjectKey);
        input[0].endpoints[2].response.clear();
        input[0].endpoints[2].code = 0;
        assert_error(service_docs(&input, &registry, Visibility::Public, &opts), ErrorCode::DuplicateObjectKey);
        input[0].endpoints[2].code = u32::MAX;
        input[0].endpoints[2].service = String::from("wrong");
        assert_error(service_docs(&input, &registry, Visibility::Public, &opts), ErrorCode::ConstraintViolation);
        input[0].endpoints[2].service = String::from("api");
        input[0].endpoints[2].roles.push(String::from("owner"));
        assert_error(service_docs(&input, &registry, Visibility::Public, &opts), ErrorCode::ConstraintViolation);
        input[0].endpoints[2].roles = vec![String::new()];
        assert_error(service_docs(&input, &registry, Visibility::Public, &opts), ErrorCode::ConstraintViolation);
    }

    #[test]
    fn catalog_identity_vectors_use_the_root_validation_errors() {
        let baseline = catalog();
        let mut vectors = Vec::new();
        let mut input = baseline.clone();
        input[0].name = String::from("bad-name");
        vectors.push((input, ErrorCode::ConstraintViolation));
        let mut input = baseline.clone();
        input.push(ServiceSpec { id: 42, name: String::from("other"), endpoints: Vec::new() });
        vectors.push((input, ErrorCode::DuplicateObjectKey));
        let mut input = baseline.clone();
        input.push(ServiceSpec { id: 43, name: String::from("api"), endpoints: Vec::new() });
        vectors.push((input, ErrorCode::DuplicateObjectKey));
        let mut input = baseline.clone();
        input[0].endpoints[2].name = String::from("ping");
        vectors.push((input, ErrorCode::DuplicateObjectKey));
        let mut input = baseline.clone();
        input[0].endpoints[2].name = String::from("bad-name");
        vectors.push((input, ErrorCode::ConstraintViolation));
        let mut input = baseline;
        input.push(ServiceSpec {
            id: 43, name: String::from("second"), endpoints: vec![EndpointSchemaSpec {
                service: String::from("second"), ..endpoint("last", 0, &["private"])
            }],
        });
        vectors.push((input, ErrorCode::DuplicateObjectKey));
        for (input, code) in vectors {
            assert_error(validate_services(&input), code);
            assert_error(service_docs(&input, &TypeRegistry::default(), Visibility::Public, &options()), code);
        }
    }

    #[test]
    fn docs_schemas_and_protocols_agree_on_visibility_codes_and_stream_separation() {
        let mut input = catalog();
        input[0].endpoints[1].params = vec![field("request", EndpointType::UInt32)];
        input[0].endpoints[1].response = vec![field("accepted", EndpointType::Boolean)];
        input[0].endpoints[1].stream_response = Some(EndpointType::Vec(Box::new(EndpointType::Int64)));
        let registry = TypeRegistry::default();
        let opts = options();
        let roles = vec![String::from("reader")];
        let visibility = Visibility::Roles(&roles);
        let document = service_docs(&input, &registry, visibility, &opts).unwrap();
        let tools = json_schema::mcp_tools(&input, &registry, visibility, &opts).unwrap();
        let open = protocol::openapi(&input, &registry, visibility, &opts).unwrap();
        let asynchronous = protocol::asyncapi(&input, &registry, visibility, &opts).unwrap();
        assert_eq!(tools.iter().map(|tool| tool.endpoint_code).collect::<Vec<_>>(), [0, 7]);
        assert!(document.text.contains("### api.watch\n\nCode: 7\n"));
        assert!(document.text.contains("| accepted | Boolean | No |"));
        assert!(document.text.contains("#### Streaming response\n\nVec&lt;Int64&gt;\n"));
        assert!(open.text.contains("\"x-jmax-code\":7"));
        assert!(open.text.contains("\"x-jmax-stream-schema\":"));
        assert!(asynchronous.text.contains("\"api.watch\":"));
        assert!(!asynchronous.text.contains("api.ping"));
        for text in [&document.text, &open.text, &asynchronous.text] {
            assert!(!text.contains("hidden"));
            assert!(!text.contains("owner"));
        }
        assert!(tools[1].output_schema.text.contains("\"accepted\""));
        assert!(!tools[1].output_schema.text.contains("\"items\""));
        let public = protocol::asyncapi(&input, &registry, Visibility::Public, &opts).unwrap();
        assert!(!public.text.contains("api.watch"));
    }

    #[test]
    fn registry_kinds_names_references_fields_and_enum_values_are_validated() {
        let vectors = vec![
            (TypeRegistry { structs: vec![EndpointType::Unit], enums: Vec::new() }, ErrorCode::ConstraintViolation),
            (TypeRegistry { structs: Vec::new(), enums: vec![structure("Wrong", Vec::new())] }, ErrorCode::ConstraintViolation),
            (TypeRegistry { structs: vec![structure("bad-name", Vec::new())], enums: Vec::new() }, ErrorCode::ConstraintViolation),
            (TypeRegistry { structs: vec![structure("Same", Vec::new()), structure("Same", Vec::new())], enums: Vec::new() }, ErrorCode::DuplicateObjectKey),
            (TypeRegistry { structs: vec![structure("Same", Vec::new())], enums: vec![enumeration("Same", &[])] }, ErrorCode::DuplicateObjectKey),
            (TypeRegistry { structs: vec![structure("Record", vec![field("", EndpointType::Unit)])], enums: Vec::new() }, ErrorCode::DuplicateObjectKey),
            (TypeRegistry { structs: vec![structure("Record", vec![field("x", EndpointType::Unit), field("x", EndpointType::String)])], enums: Vec::new() }, ErrorCode::DuplicateObjectKey),
            (TypeRegistry { structs: Vec::new(), enums: vec![enumeration("State", &[("", 0)])] }, ErrorCode::ConstraintViolation),
            (TypeRegistry { structs: Vec::new(), enums: vec![enumeration("State", &[("bad-name", 0)])] }, ErrorCode::ConstraintViolation),
            (TypeRegistry { structs: Vec::new(), enums: vec![enumeration("State", &[("A", 0), ("A", 1)])] }, ErrorCode::DuplicateObjectKey),
            (TypeRegistry { structs: Vec::new(), enums: vec![enumeration("State", &[("A", 0), ("B", 0)])] }, ErrorCode::DuplicateObjectKey),
            (TypeRegistry { structs: vec![structure("Record", vec![field("next", EndpointType::StructRef(String::from("Missing")))])], enums: Vec::new() }, ErrorCode::ConstraintViolation),
            (TypeRegistry { structs: vec![structure("Record", vec![field("next", EndpointType::EnumRef { name: String::from("Record"), prefixed_name: false })])], enums: Vec::new() }, ErrorCode::ConstraintViolation),
            (TypeRegistry { structs: vec![structure("Record", vec![field("next", EndpointType::StructTable { struct_ref: String::from("invalid name") })])], enums: Vec::new() }, ErrorCode::ConstraintViolation),
        ];
        for (registry, expected) in vectors {
            assert_error(service_docs(&[], &registry, Visibility::Public, &options()), expected);
            assert_error(json_schema::validate_registry(&registry, options().max_type_depth), expected);
        }
    }

    #[test]
    fn inline_definitions_are_indexed_catalog_wide_and_checked_for_conflicts() {
        let definition = structure("Record", vec![field("id", EndpointType::UInt32)]);
        let registry = TypeRegistry { structs: vec![definition.clone()], enums: Vec::new() };
        let mut input = catalog();
        input[0].endpoints[0].params = vec![field("record", definition.clone())];
        assert!(service_docs(&input, &registry, Visibility::Public, &options()).is_ok());
        input[0].endpoints[0].params = vec![field("record", structure("Record", vec![field("id", EndpointType::String)]))];
        assert_error(service_docs(&input, &registry, Visibility::Public, &options()), ErrorCode::DuplicateObjectKey);
        input[0].endpoints[0].params = vec![field("record", EndpointType::StructRef(String::from("Record")))];
        input[0].endpoints[2].response = vec![field("record", definition.clone())];
        let document = service_docs(&input, &TypeRegistry::default(), Visibility::Public, &options()).unwrap();
        assert!(document.text.contains("StructRef Record"));
        assert!(!document.text.contains("hidden"));
        input[0].endpoints[1].params = vec![field("record", definition)];
        assert_error(service_docs(&input, &TypeRegistry::default(), Visibility::Public, &options()), ErrorCode::DuplicateObjectKey);
    }

    #[test]
    fn structural_depth_limits_include_hidden_types_but_do_not_expand_named_cycles() {
        let mut input = catalog();
        let mut opts = options();
        opts.max_type_depth = 0;
        assert_error(service_docs(&[], &TypeRegistry::default(), Visibility::All, &opts), ErrorCode::ConstraintViolation);
        opts.max_type_depth = 1;
        input[0].endpoints[2].stream_response = Some(EndpointType::Optional(Box::new(EndpointType::Unit)));
        assert_error(service_docs(&input, &TypeRegistry::default(), Visibility::Public, &opts), ErrorCode::DepthLimitExceeded);
        opts.max_type_depth = 2;
        assert!(service_docs(&input, &TypeRegistry::default(), Visibility::Public, &opts).is_ok());
        let registry = TypeRegistry { structs: vec![structure("Cycle", vec![field("next", EndpointType::StructRef(String::from("Cycle")))])], enums: Vec::new() };
        input[0].endpoints[0].params = vec![field("cycle", EndpointType::StructRef(String::from("Cycle")))];
        let document = service_docs(&input, &registry, Visibility::All, &opts).unwrap();
        assert_eq!(document.text.matches("StructRef Cycle").count(), 1);
        opts.max_type_depth = 1;
        assert_error(service_docs(&input, &registry, Visibility::Public, &opts), ErrorCode::DepthLimitExceeded);
        opts.max_type_depth = u16::MAX;
        assert!(service_docs(&input, &registry, Visibility::All, &opts).is_ok());
        let empty_registry = TypeRegistry::default();
        let primitive = EndpointType::Unit;
        let mut context = Context::registry(&empty_registry, 1).unwrap();
        // A primitive root at exactly the boundary is permitted.
        assert!(context.visit(Visit::Type(&primitive, 1, false)).is_ok());
    }

    #[test]
    fn deep_owned_trees_use_explicit_stacks_for_validation_equality_and_rendering() {
        let mut ty = EndpointType::String;
        for _ in 0..256 { ty = EndpointType::Vec(Box::new(ty)); }
        let definition = structure("Deep", vec![field("values", ty)]);
        let registry = TypeRegistry { structs: vec![definition.clone()], enums: Vec::new() };
        let mut input = catalog();
        input[0].endpoints[0].params = vec![field("deep", definition)];
        let mut opts = options();
        opts.max_type_depth = 258;
        let document = service_docs(&input, &registry, Visibility::Public, &opts).unwrap();
        assert_eq!(document.text.matches("Vec&lt;").count(), 256);
        opts.max_type_depth = 257;
        assert_error(service_docs(&input, &registry, Visibility::Public, &opts), ErrorCode::DepthLimitExceeded);
        opts.max_type_depth = 258;
        input[0].endpoints[0].params[0].ty = EndpointType::Optional(Box::new(registry.structs[0].clone()));
        // An equal registered inline copy must still fit at its use site.
        assert_error(service_docs(&input, &registry, Visibility::Public, &opts), ErrorCode::DepthLimitExceeded);
        opts.max_type_depth = 259;
        assert!(service_docs(&input, &registry, Visibility::Public, &opts).is_ok());
    }

    #[test]
    fn errors_have_exact_signed_boundaries_escaped_cells_and_stable_input_order() {
        let errors = vec![
            ErrorDoc { code: i32::MAX, name: String::from("Last"), description: String::from("<b>|\r\n`x`") },
            ErrorDoc { code: 0, name: String::from("_Zero"), description: String::new() },
            ErrorDoc { code: i32::MIN, name: String::from("First"), description: String::from("雪") },
        ];
        let snapshot = errors.clone();
        let expected = concat!(
            "# Errors\n\n| Code | Name | Description |\n| --- | --- | --- |\n",
            "| 2147483647 | Last | &lt;b&gt;\\|  \\`x\\` |\n",
            "| 0 | \\_Zero |  |\n",
            "| -2147483648 | First | 雪 |\n",
        );
        let document = error_docs(&errors, u32::MAX).unwrap();
        assert_eq!(document.text, expected);
        assert_document(&document, "text/markdown");
        assert_eq!(error_docs(&errors, u32::MAX).unwrap(), document);
        assert_eq!(errors, snapshot);
    }

    #[test]
    fn invalid_error_names_and_duplicate_error_identities_are_rejected_before_output() {
        for name in ["", "9name", "bad-name", "two words", "<raw>", "é", "a\nB", "a\0"] {
            assert_error(error_docs(&[ErrorDoc {
                code: 0, name: String::from(name), description: String::new(),
            }], 0), ErrorCode::ConstraintViolation);
        }
        for (name, code) in [("B", 1), ("A", 2)] {
            let errors = vec![
                ErrorDoc { code: 1, name: String::from("A"), description: String::new() },
                ErrorDoc { code, name: String::from(name), description: String::new() },
            ];
            assert_error(error_docs(&errors, 0), ErrorCode::DuplicateObjectKey);
        }
    }

    #[test]
    fn systemd_minimal_and_full_units_have_exact_directive_order_and_media_type() {
        let mut opts = unit_options();
        let document = systemd_unit(&opts, u32::MAX).unwrap();
        assert_eq!(document.text, concat!(
            "[Unit]\nDescription=API server\nAfter=network-online.target\n",
            "\n[Service]\nType=simple\nExecStart=\"/usr/bin/jmax\"\n",
            "Restart=on-failure\nRestartSec=5\n",
            "\n[Install]\nWantedBy=multi-user.target\n",
        ));
        assert_document(&document, "text/plain");
        opts.arguments = vec![String::from("--listen"), String::from("127.0.0.1:8080"), String::new()];
        opts.user = Some(String::from("jmax-worker_7"));
        opts.working_directory = Some(String::from("/srv/jmax data"));
        opts.restart_seconds = u32::MAX;
        let snapshot = opts.clone();
        let document = systemd_unit(&opts, u32::MAX).unwrap();
        assert_eq!(document.text, concat!(
            "[Unit]\nDescription=API server\nAfter=network-online.target\n",
            "\n[Service]\nType=simple\nExecStart=\"/usr/bin/jmax\" \"--listen\" \"127.0.0.1:8080\" \"\"\n",
            "User=jmax-worker_7\nWorkingDirectory=/srv/jmax data\n",
            "Restart=on-failure\nRestartSec=4294967295\n",
            "\n[Install]\nWantedBy=multi-user.target\n",
        ));
        assert_eq!(systemd_unit(&opts, u32::MAX).unwrap(), document);
        assert_eq!(opts, snapshot);
    }

    #[test]
    fn exec_words_protect_expansion_and_preserve_empty_whitespace_and_shell_delimiters() {
        for (input, expected) in [
            ("", r#""""#),
            ("plain", r#""plain""#),
            ("two words", r#""two words""#),
            (" ", r#"" ""#),
            ("%n%", r#""%%n%%""#),
            ("$HOME ${USER}", r#""$$HOME $${USER}""#),
            ("a\\b\"c", r#""a\\b\"c""#),
            (";", r#"";""#),
            ("|&<>#'", r#""|&<>#'""#),
            ("\\", r#""\\""#),
            ("雪😀", "\"雪😀\""),
        ] {
            let mut out = Writer::new(expected.len() as u32);
            out.exec_word(input).unwrap();
            assert_eq!(out.text, expected);
            assert_error(Writer::new(expected.len() as u32 - 1).exec_word(input), ErrorCode::PayloadTooLarge);
        }
        let mut opts = unit_options();
        opts.description = String::from("100% ready $HOME");
        opts.executable = String::from("/opt/my app/%n/$bin\\\"雪");
        opts.arguments = vec![String::from("${HOME} %i \\ \" ; #"), String::from(";"), String::new()];
        opts.working_directory = Some(String::from("/srv/%n/$HOME data"));
        let document = systemd_unit(&opts, u32::MAX).unwrap();
        assert!(document.text.contains("Description=100%% ready $HOME\n"));
        assert!(document.text.contains(concat!(
            "ExecStart=\"/opt/my app/%%n/$$bin\\\\\\\"雪\" ",
            "\"$${HOME} %%i \\\\ \\\" ; #\" \";\" \"\"\n",
        )));
        assert!(document.text.contains("WorkingDirectory=/srv/%%n/$HOME data\n"));
    }

    #[test]
    fn scalar_directives_preserve_literals_without_continuing_into_the_next_directive() {
        let mut opts = unit_options();
        opts.description = String::from("literal\\");
        opts.working_directory = Some(String::from("/srv/final\\"));
        let document = systemd_unit(&opts, u32::MAX).unwrap();
        assert!(document.text.contains("Description=literal\\ \nAfter=network-online.target\n"));
        assert!(document.text.contains("WorkingDirectory=/srv/final\\ \nRestart=on-failure\n"));
        for line in document.text.lines() { assert!(!line.ends_with('\\')); }
        opts.working_directory = Some(String::from("/srv/final "));
        let document = systemd_unit(&opts, u32::MAX).unwrap();
        assert!(document.text.contains("WorkingDirectory=/srv/final /.\n"));
        opts.working_directory = Some(String::from("/srv/a\"b\\c"));
        let document = systemd_unit(&opts, u32::MAX).unwrap();
        assert!(document.text.contains("WorkingDirectory=/srv/a\"b\\c\n"));
    }

    #[test]
    fn all_control_characters_are_rejected_in_every_systemd_input() {
        for ch in (0u32..=159).filter_map(char::from_u32).filter(|ch| ch.is_control()) {
            for slot in 0..5 {
                let mut opts = unit_options();
                let mut text = String::from("/safe");
                text.push(ch);
                match slot {
                    0 => opts.description = text,
                    1 => opts.executable = text,
                    2 => opts.arguments = vec![text],
                    3 => opts.user = Some(text),
                    _ => opts.working_directory = Some(text),
                }
                assert_error(systemd_unit(&opts, 0), ErrorCode::ConstraintViolation);
            }
        }
        for injection in ["\nUser=root", "\r\n[Install]", "\0ExecStart=/bad", "\t"] {
            let mut opts = unit_options();
            opts.description = String::from(injection);
            assert_error(systemd_unit(&opts, u32::MAX), ErrorCode::ConstraintViolation);
        }
    }

    #[test]
    fn relative_paths_and_invalid_users_are_rejected_and_restart_zero_is_valid() {
        for path in ["", "jmax", "./jmax", "../jmax", "C:\\jmax", "~/jmax", "%h/jmax"] {
            let mut opts = unit_options();
            opts.executable = String::from(path);
            assert_error(systemd_unit(&opts, 0), ErrorCode::ConstraintViolation);
            opts = unit_options();
            opts.working_directory = Some(String::from(path));
            assert_error(systemd_unit(&opts, 0), ErrorCode::ConstraintViolation);
        }
        for user in ["", "two words", "root;bad", "%i", "$USER", "a.b", "é", "a/b", "root=admin"] {
            let mut opts = unit_options();
            opts.user = Some(String::from(user));
            assert_error(systemd_unit(&opts, u32::MAX), ErrorCode::ConstraintViolation);
        }
        for user in ["root", "_jmax", "jmax-worker_7", "123", "-"] {
            let mut opts = unit_options();
            opts.user = Some(String::from(user));
            opts.restart_seconds = 0;
            let document = systemd_unit(&opts, u32::MAX).unwrap();
            assert!(document.text.contains("RestartSec=0\n"));
        }
    }

    #[test]
    fn every_document_boundary_counts_escaped_utf8_and_the_final_newline() {
        let mut input = catalog();
        input[0].endpoints[0].description = String::from("<雪>|\\\r\n😀");
        input[0].endpoints[0].params = vec![field("a|b", EndpointType::Optional(Box::new(EndpointType::String)))];
        let registry = TypeRegistry::default();
        let opts = options();
        let expected = service_docs(&input, &registry, Visibility::All, &opts).unwrap();
        for limit in 0..=expected.text.len() + 1 {
            let mut limited = opts.clone();
            limited.max_output_bytes = limit as u32;
            let actual = service_docs(&input, &registry, Visibility::All, &limited);
            if limit < expected.text.len() {
                assert_error(actual, ErrorCode::PayloadTooLarge);
            } else {
                assert_eq!(actual.unwrap(), expected);
            }
        }
        let errors = vec![ErrorDoc { code: i32::MIN, name: String::from("A_B"), description: String::from("雪&<>😀") }];
        let expected = error_docs(&errors, u32::MAX).unwrap();
        for limit in 0..=expected.text.len() + 1 {
            let actual = error_docs(&errors, limit as u32);
            if limit < expected.text.len() { assert_error(actual, ErrorCode::PayloadTooLarge); }
            else { assert_eq!(actual.unwrap(), expected); }
        }
        let mut unit = unit_options();
        unit.description = String::from("雪%😀");
        unit.arguments = vec![String::from("$%\\\"雪")];
        let expected = systemd_unit(&unit, u32::MAX).unwrap();
        for limit in 0..=expected.text.len() + 1 {
            let actual = systemd_unit(&unit, limit as u32);
            if limit < expected.text.len() { assert_error(actual, ErrorCode::PayloadTooLarge); }
            else { assert_eq!(actual.unwrap(), expected); }
        }
    }

    #[test]
    fn hidden_metadata_and_unused_definitions_do_not_consume_the_document_budget() {
        let mut input = catalog();
        let mut opts = options();
        let expected = service_docs(&input, &TypeRegistry::default(), Visibility::Public, &opts).unwrap();
        opts.max_output_bytes = expected.text.len() as u32;
        input[0].endpoints[2].description = "hidden <metadata>|".repeat(4096);
        input[0].endpoints[2].params = vec![EndpointField {
            name: String::from("private_field"), ty: EndpointType::String,
            description: "large field description".repeat(4096),
        }];
        let registry = TypeRegistry {
            structs: vec![structure("Unused", vec![EndpointField {
                name: String::from("value"), ty: EndpointType::String,
                description: "unused registered metadata".repeat(4096),
            }])],
            enums: Vec::new(),
        };
        assert_eq!(service_docs(&input, &registry, Visibility::Public, &opts).unwrap(), expected);
        assert_error(service_docs(&input, &registry, Visibility::All, &opts), ErrorCode::PayloadTooLarge);
    }

    #[test]
    fn writer_append_and_integer_boundaries_are_exact_without_partial_numeric_writes() {
        for (value, expected) in [
            (i64::MIN, "-9223372036854775808"),
            (i64::MAX, "9223372036854775807"),
            (i64::from(u32::MAX), "4294967295"),
            (i64::from(i32::MIN), "-2147483648"),
            (-1, "-1"), (0, "0"), (9, "9"), (10, "10"),
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
        out.raw("").unwrap();
        assert_eq!(out.text.len(), 4);
        assert_error(out.character('😀'), ErrorCode::PayloadTooLarge);
        let mut out = Writer::new(0);
        out.raw("").unwrap();
        assert_error(out.raw("x"), ErrorCode::PayloadTooLarge);
        assert_error::<()>(Err(allocation_error()), ErrorCode::BudgetExhausted);
    }
}
