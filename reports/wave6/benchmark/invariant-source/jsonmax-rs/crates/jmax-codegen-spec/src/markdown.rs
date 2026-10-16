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
