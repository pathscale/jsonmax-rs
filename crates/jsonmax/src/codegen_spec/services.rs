//! Ordered service selection and bounded, lossless JSON service catalogs.

use super::*;
use alloc::boxed::Box;

/// Returns public endpoints, omitting services without public endpoints.
///
/// The entire catalog is validated before any endpoints are hidden. Returned
/// endpoints own independent copies of all metadata and inline types.
pub fn public_services(services: &[ServiceSpec]) -> Result<Vec<ServiceSpec>> {
    validate_services(services)?;
    select_validated(services, Visibility::Public)
}

/// Returns every endpoint in input order, omitting empty services.
pub fn all_services(services: &[ServiceSpec]) -> Result<Vec<ServiceSpec>> {
    validate_services(services)?;
    select_validated(services, Visibility::All)
}

/// Selects whole endpoints using exact role matching, preserving input order.
///
/// Public endpoints are also included by `Visibility::Roles`. Invalid hidden
/// catalog identities are rejected, and the input is never modified.
pub fn select_services(
    services: &[ServiceSpec],
    visibility: Visibility<'_>,
) -> Result<Vec<ServiceSpec>> {
    validate_services(services)?;
    select_validated(services, visibility)
}

/// Serializes the visible catalog as deterministic `application/json`.
///
/// Type tags and member order follow the ABI, without resolving named type
/// references. A root type has depth one; each inline child adds one. Output
/// limits count UTF-8 bytes, including the single terminating newline, and are
/// enforced before every append. `server_url` is not catalog metadata.
pub fn services_document(
    services: &[ServiceSpec],
    visibility: Visibility<'_>,
    options: &DocumentOptions,
) -> Result<GeneratedDocument> {
    validate_services(services)?;
    if options.max_type_depth == 0 {
        return Err(spec_error(
            ErrorCode::ConstraintViolation,
            "maximum type depth must be nonzero",
        ));
    }

    let mut out = CatalogWriter::new(options.max_output_bytes);
    out.raw("{\"title\":")?;
    out.string(&options.title)?;
    out.raw(",\"version\":")?;
    out.string(&options.version)?;
    out.raw(",\"description\":")?;
    out.string(&options.description)?;
    out.raw(",\"services\":[")?;
    let mut first_service = true;
    for service in services {
        if !service.endpoints.iter().any(|endpoint| visibility.includes(endpoint)) {
            continue;
        }
        out.separator(&mut first_service)?;
        out.raw("{\"id\":")?;
        out.integer(i64::from(service.id))?;
        out.raw(",\"name\":")?;
        out.string(&service.name)?;
        out.raw(",\"endpoints\":[")?;
        let mut first_endpoint = true;
        for endpoint in &service.endpoints {
            if !visibility.includes(endpoint) {
                continue;
            }
            out.separator(&mut first_endpoint)?;
            out.raw("{\"code\":")?;
            out.integer(i64::from(endpoint.code))?;
            out.raw(",\"name\":")?;
            out.string(&endpoint.name)?;
            out.raw(",\"service\":")?;
            out.string(&endpoint.service)?;
            out.raw(",\"roles\":[")?;
            for (index, role) in endpoint.roles.iter().enumerate() {
                if index != 0 {
                    out.raw(",")?;
                }
                out.string(role)?;
            }
            out.raw("],\"params\":")?;
            write_fields(&mut out, &endpoint.params, options.max_type_depth)?;
            out.raw(",\"response\":")?;
            write_fields(&mut out, &endpoint.response, options.max_type_depth)?;
            out.raw(",\"stream_response\":")?;
            match &endpoint.stream_response {
                Some(ty) => write_type(&mut out, ty, options.max_type_depth)?,
                None => out.raw("null")?,
            }
            out.raw(",\"description\":")?;
            out.string(&endpoint.description)?;
            out.raw("}")?;
        }
        out.raw("]}")?;
    }
    out.raw("]}\n")?;
    GeneratedDocument::new("application/json", out.text, options.max_output_bytes)
}

fn allocation_error() -> Error {
    spec_error(ErrorCode::BudgetExhausted, "service catalog allocation failed")
}

fn reserved_vec<T>(capacity: usize) -> Result<Vec<T>> {
    let mut values = Vec::new();
    values.try_reserve_exact(capacity).map_err(|_| allocation_error())?;
    Ok(values)
}

fn push<T>(values: &mut Vec<T>, value: T) -> Result<()> {
    values.try_reserve(1).map_err(|_| allocation_error())?;
    values.push(value);
    Ok(())
}

fn copy_string(value: &str) -> Result<String> {
    let mut copied = String::new();
    copied.try_reserve_exact(value.len()).map_err(|_| allocation_error())?;
    copied.push_str(value);
    Ok(copied)
}

fn select_validated(
    services: &[ServiceSpec],
    visibility: Visibility<'_>,
) -> Result<Vec<ServiceSpec>> {
    let mut selected = Vec::new();
    for service in services {
        let count = service.endpoints.iter().filter(|endpoint| visibility.includes(endpoint)).count();
        if count == 0 {
            continue;
        }
        let mut endpoints = reserved_vec(count)?;
        for endpoint in &service.endpoints {
            if visibility.includes(endpoint) {
                endpoints.push(copy_endpoint(endpoint)?);
            }
        }
        push(&mut selected, ServiceSpec {
            id: service.id,
            name: copy_string(&service.name)?,
            endpoints,
        })?;
    }
    Ok(selected)
}

fn copy_endpoint(endpoint: &EndpointSchemaSpec) -> Result<EndpointSchemaSpec> {
    let mut roles = reserved_vec(endpoint.roles.len())?;
    for role in &endpoint.roles {
        roles.push(copy_string(role)?);
    }
    Ok(EndpointSchemaSpec {
        code: endpoint.code,
        name: copy_string(&endpoint.name)?,
        service: copy_string(&endpoint.service)?,
        roles,
        params: copy_fields(&endpoint.params)?,
        response: copy_fields(&endpoint.response)?,
        stream_response: endpoint.stream_response.as_ref().map(copy_type).transpose()?,
        description: copy_string(&endpoint.description)?,
    })
}

fn copy_fields(fields: &[EndpointField]) -> Result<Vec<EndpointField>> {
    let mut copied = reserved_vec(fields.len())?;
    for field in fields {
        copied.push(EndpointField {
            name: copy_string(&field.name)?,
            ty: copy_type(&field.ty)?,
            description: copy_string(&field.description)?,
        });
    }
    Ok(copied)
}

// Use the global allocator's fallible interface for owned ABI wrapper types.
fn boxed_type(value: EndpointType) -> Result<Box<EndpointType>> {
    let layout = core::alloc::Layout::new::<EndpointType>();
    // SAFETY: EndpointType is a nonzero-sized enum, and Layout::new supplies its
    // valid size and alignment. A null allocation is handled before any access.
    let raw = unsafe { alloc::alloc::alloc(layout) }.cast::<EndpointType>();
    if raw.is_null() {
        return Err(allocation_error());
    }
    // SAFETY: raw is a fresh, aligned global allocation for one EndpointType.
    // write initializes it by moving value, then from_raw transfers sole
    // ownership to a Box using that same global allocator and layout. No
    // fallible operations or retained aliases occur between these steps.
    Ok(unsafe {
        raw.write(value);
        Box::from_raw(raw)
    })
}

enum CopyFrame<'a> {
    Vector,
    Optional,
    Struct {
        name: String,
        source: &'a [EndpointField],
        fields: Vec<EndpointField>,
    },
}

// Explicit continuations avoid recursion while copying arbitrarily nested ABI
// values. Selection has no depth option and must preserve the entire type tree.
fn copy_type(ty: &EndpointType) -> Result<EndpointType> {
    let mut frames = Vec::new();
    let mut current = ty;
    'visit: loop {
        let mut copied = match current {
            EndpointType::Vec(child) => {
                push(&mut frames, CopyFrame::Vector)?;
                current = child;
                continue;
            }
            EndpointType::Optional(child) => {
                push(&mut frames, CopyFrame::Optional)?;
                current = child;
                continue;
            }
            EndpointType::Struct { name, fields } => {
                let name = copy_string(name)?;
                let copied_fields = reserved_vec(fields.len())?;
                if let Some(first) = fields.first() {
                    push(&mut frames, CopyFrame::Struct {
                        name, source: fields, fields: copied_fields,
                    })?;
                    current = &first.ty;
                    continue;
                }
                EndpointType::Struct { name, fields: copied_fields }
            }
            EndpointType::StructRef(name) => EndpointType::StructRef(copy_string(name)?),
            EndpointType::StructTable { struct_ref } => EndpointType::StructTable {
                struct_ref: copy_string(struct_ref)?,
            },
            EndpointType::Enum { name, variants } => {
                let mut copied_variants = reserved_vec(variants.len())?;
                for variant in variants {
                    copied_variants.push(EnumVariantSpec {
                        name: copy_string(&variant.name)?,
                        value: variant.value,
                        description: copy_string(&variant.description)?,
                    });
                }
                EndpointType::Enum { name: copy_string(name)?, variants: copied_variants }
            }
            EndpointType::EnumRef { name, prefixed_name } => EndpointType::EnumRef {
                name: copy_string(name)?, prefixed_name: *prefixed_name,
            },
            EndpointType::NanoId { len } => EndpointType::NanoId { len: *len },
            EndpointType::UInt32 | EndpointType::Int32 | EndpointType::Int64
            | EndpointType::Float64 | EndpointType::Boolean | EndpointType::String
            | EndpointType::Bytea | EndpointType::UUID | EndpointType::IpAddr
            | EndpointType::Object | EndpointType::Unit | EndpointType::TimeStampMs
            | EndpointType::BlockchainDecimal | EndpointType::BlockchainAddress
            | EndpointType::BlockchainTransactionHash => current.clone(),
        };
        loop {
            match frames.pop() {
                None => return Ok(copied),
                Some(CopyFrame::Vector) => copied = EndpointType::Vec(boxed_type(copied)?),
                Some(CopyFrame::Optional) => copied = EndpointType::Optional(boxed_type(copied)?),
                Some(CopyFrame::Struct { name, source, mut fields }) => {
                    let field = &source[fields.len()];
                    fields.push(EndpointField {
                        name: copy_string(&field.name)?,
                        ty: copied,
                        description: copy_string(&field.description)?,
                    });
                    if fields.len() < source.len() {
                        current = &source[fields.len()].ty;
                        push(&mut frames, CopyFrame::Struct { name, source, fields })?;
                        continue 'visit;
                    }
                    copied = EndpointType::Struct { name, fields };
                }
            }
        }
    }
}

struct CatalogWriter {
    text: String,
    limit: usize,
}

impl CatalogWriter {
    fn new(limit: u32) -> Self {
        Self { text: String::new(), limit: limit as usize }
    }

    fn raw(&mut self, text: &str) -> Result<()> {
        if text.len() > self.limit.saturating_sub(self.text.len()) {
            return Err(spec_error(ErrorCode::PayloadTooLarge, "service catalog exceeds output limit"));
        }
        self.text.try_reserve(text.len()).map_err(|_| allocation_error())?;
        self.text.push_str(text);
        Ok(())
    }

    fn separator(&mut self, first: &mut bool) -> Result<()> {
        if *first {
            *first = false;
            Ok(())
        } else {
            self.raw(",")
        }
    }

    fn string(&mut self, text: &str) -> Result<()> {
        self.raw("\"")?;
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
            // Every escape starts at an ASCII byte, hence at a UTF-8 boundary.
            self.raw(&text[start..index])?;
            if escape.is_empty() {
                const HEX: &[u8; 16] = b"0123456789abcdef";
                let escaped = [b'\\', b'u', b'0', b'0', HEX[(byte >> 4) as usize], HEX[(byte & 15) as usize]];
                let escaped = core::str::from_utf8(&escaped).map_err(|_| {
                    spec_error(ErrorCode::InternalError, "invalid ASCII string escape")
                })?;
                self.raw(escaped)?;
            } else {
                self.raw(escape)?;
            }
            start = index + 1;
        }
        self.raw(&text[start..])?;
        self.raw("\"")
    }

    fn integer(&mut self, value: i64) -> Result<()> {
        let mut bytes = [0u8; 20];
        let mut offset = bytes.len();
        let mut magnitude = value.unsigned_abs();
        loop {
            offset -= 1;
            bytes[offset] = b'0' + (magnitude % 10) as u8;
            magnitude /= 10;
            if magnitude == 0 {
                break;
            }
        }
        if value < 0 {
            offset -= 1;
            bytes[offset] = b'-';
        }
        let text = core::str::from_utf8(&bytes[offset..]).map_err(|_| {
            spec_error(ErrorCode::InternalError, "invalid ASCII integer")
        })?;
        self.raw(text)
    }
}

enum TypeTask<'a> {
    Type(&'a EndpointType, u32),
    Fields(&'a [EndpointField], u32),
    FieldTail(&'a str),
    Variants(&'a [EnumVariantSpec]),
    Raw(&'static str),
}

fn write_fields(out: &mut CatalogWriter, fields: &[EndpointField], max_depth: u16) -> Result<()> {
    let mut tasks = Vec::new();
    out.raw("[")?;
    push(&mut tasks, TypeTask::Fields(fields, 1))?;
    write_tasks(out, &mut tasks, max_depth)
}

fn write_type(out: &mut CatalogWriter, ty: &EndpointType, max_depth: u16) -> Result<()> {
    let mut tasks = Vec::new();
    push(&mut tasks, TypeTask::Type(ty, 1))?;
    write_tasks(out, &mut tasks, max_depth)
}

// Arrays retain only their unvisited suffix. Auxiliary memory is proportional
// to inline depth rather than the number of fields or enum variants.
fn write_tasks<'a>(
    out: &mut CatalogWriter,
    tasks: &mut Vec<TypeTask<'a>>,
    max_depth: u16,
) -> Result<()> {
    while let Some(task) = tasks.pop() {
        match task {
            TypeTask::Raw(text) => out.raw(text)?,
            TypeTask::FieldTail(description) => {
                out.raw(",\"description\":")?;
                out.string(description)?;
                out.raw("}")?;
            }
            TypeTask::Fields(fields, depth) => {
                if let Some((field, rest)) = fields.split_first() {
                    out.raw("{\"name\":")?;
                    out.string(&field.name)?;
                    out.raw(",\"ty\":")?;
                    push(tasks, TypeTask::Fields(rest, depth))?;
                    if !rest.is_empty() {
                        push(tasks, TypeTask::Raw(","))?;
                    }
                    push(tasks, TypeTask::FieldTail(&field.description))?;
                    push(tasks, TypeTask::Type(&field.ty, depth))?;
                } else {
                    out.raw("]")?;
                }
            }
            TypeTask::Variants(variants) => {
                if let Some((variant, rest)) = variants.split_first() {
                    out.raw("{\"name\":")?;
                    out.string(&variant.name)?;
                    out.raw(",\"value\":")?;
                    out.integer(i64::from(variant.value))?;
                    out.raw(",\"description\":")?;
                    out.string(&variant.description)?;
                    out.raw("}")?;
                    if !rest.is_empty() {
                        out.raw(",")?;
                    }
                    push(tasks, TypeTask::Variants(rest))?;
                } else {
                    out.raw("]")?;
                }
            }
            TypeTask::Type(ty, depth) => {
                if depth > u32::from(max_depth) {
                    return Err(spec_error(ErrorCode::DepthLimitExceeded, "inline service type exceeds depth limit"));
                }
                let kind = match ty {
                    EndpointType::UInt32 => "UInt32",
                    EndpointType::Int32 => "Int32",
                    EndpointType::Int64 => "Int64",
                    EndpointType::Float64 => "Float64",
                    EndpointType::Boolean => "Boolean",
                    EndpointType::String => "String",
                    EndpointType::Bytea => "Bytea",
                    EndpointType::UUID => "UUID",
                    EndpointType::NanoId { .. } => "NanoId",
                    EndpointType::IpAddr => "IpAddr",
                    EndpointType::Struct { .. } => "Struct",
                    EndpointType::StructRef(_) => "StructRef",
                    EndpointType::Object => "Object",
                    EndpointType::StructTable { .. } => "StructTable",
                    EndpointType::Vec(_) => "Vec",
                    EndpointType::Unit => "Unit",
                    EndpointType::Optional(_) => "Optional",
                    EndpointType::Enum { .. } => "Enum",
                    EndpointType::EnumRef { .. } => "EnumRef",
                    EndpointType::TimeStampMs => "TimeStampMs",
                    EndpointType::BlockchainDecimal => "BlockchainDecimal",
                    EndpointType::BlockchainAddress => "BlockchainAddress",
                    EndpointType::BlockchainTransactionHash => "BlockchainTransactionHash",
                };
                out.raw("{\"kind\":")?;
                out.string(kind)?;
                push(tasks, TypeTask::Raw("}"))?;
                match ty {
                    EndpointType::NanoId { len } => {
                        out.raw(",\"len\":")?;
                        out.integer(i64::from(*len))?;
                    }
                    EndpointType::Struct { name, fields } => {
                        out.raw(",\"name\":")?;
                        out.string(name)?;
                        out.raw(",\"fields\":[")?;
                        push(tasks, TypeTask::Fields(fields, depth + 1))?;
                    }
                    EndpointType::StructRef(name) => {
                        out.raw(",\"value\":")?;
                        out.string(name)?;
                    }
                    EndpointType::StructTable { struct_ref } => {
                        out.raw(",\"struct_ref\":")?;
                        out.string(struct_ref)?;
                    }
                    EndpointType::Vec(child) | EndpointType::Optional(child) => {
                        out.raw(",\"value\":")?;
                        push(tasks, TypeTask::Type(child, depth + 1))?;
                    }
                    EndpointType::Enum { name, variants } => {
                        out.raw(",\"name\":")?;
                        out.string(name)?;
                        out.raw(",\"variants\":[")?;
                        push(tasks, TypeTask::Variants(variants))?;
                    }
                    EndpointType::EnumRef { name, prefixed_name } => {
                        out.raw(",\"name\":")?;
                        out.string(name)?;
                        out.raw(",\"prefixed_name\":")?;
                        out.raw(if *prefixed_name { "true" } else { "false" })?;
                    }
                    EndpointType::UInt32 | EndpointType::Int32 | EndpointType::Int64
                    | EndpointType::Float64 | EndpointType::Boolean | EndpointType::String
                    | EndpointType::Bytea | EndpointType::UUID | EndpointType::IpAddr
                    | EndpointType::Object | EndpointType::Unit | EndpointType::TimeStampMs
                    | EndpointType::BlockchainDecimal | EndpointType::BlockchainAddress
                    | EndpointType::BlockchainTransactionHash => {}
                }
            }
        }
    }
    Ok(())
}
