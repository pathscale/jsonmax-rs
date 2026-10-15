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

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn field(name: &str, ty: EndpointType) -> EndpointField {
        EndpointField {
            name: String::from(name), ty, description: String::from("field description"),
        }
    }

    fn endpoint(service: &str, name: &str, code: u32, roles: &[&str]) -> EndpointSchemaSpec {
        EndpointSchemaSpec {
            code,
            name: String::from(name),
            service: String::from(service),
            roles: roles.iter().map(|role| String::from(*role)).collect(),
            params: vec![field("request", EndpointType::String)],
            response: vec![field("accepted", EndpointType::Boolean)],
            stream_response: Some(EndpointType::Optional(Box::new(EndpointType::Int64))),
            description: String::from("endpoint description"),
        }
    }

    fn catalog() -> Vec<ServiceSpec> {
        vec![
            ServiceSpec {
                id: 9,
                name: String::from("market"),
                endpoints: vec![
                    endpoint("market", "subscribe", 20, &["reader", "admin"]),
                    endpoint("market", "ping", 0, &[]),
                    endpoint("market", "manage", 21, &["admin"]),
                ],
            },
            ServiceSpec { id: 0, name: String::from("empty"), endpoints: Vec::new() },
            ServiceSpec {
                id: u16::MAX,
                name: String::from("system"),
                endpoints: vec![endpoint("system", "health", u32::MAX, &[])],
            },
            ServiceSpec {
                id: 3,
                name: String::from("private"),
                endpoints: vec![endpoint("private", "secret", 22, &["root"])],
            },
        ]
    }

    fn assert_error<T: core::fmt::Debug>(result: Result<T>, code: ErrorCode) -> Error {
        let error = result.unwrap_err();
        assert_eq!(error.code, code);
        assert_eq!(error.source, ErrorSource::Encode);
        assert_eq!(error.byte_offset, 0);
        assert_eq!(error.schema_id, 0);
        assert_eq!(error.field_id, 0);
        error
    }

    fn type_text(ty: &EndpointType, max_depth: u16) -> Result<String> {
        let mut out = CatalogWriter::new(1_000_000);
        write_type(&mut out, ty, max_depth)?;
        Ok(out.text)
    }

    fn type_vectors() -> Vec<(EndpointType, &'static str)> {
        vec![
            (EndpointType::UInt32, "{\"kind\":\"UInt32\"}"),
            (EndpointType::Int32, "{\"kind\":\"Int32\"}"),
            (EndpointType::Int64, "{\"kind\":\"Int64\"}"),
            (EndpointType::Float64, "{\"kind\":\"Float64\"}"),
            (EndpointType::Boolean, "{\"kind\":\"Boolean\"}"),
            (EndpointType::String, "{\"kind\":\"String\"}"),
            (EndpointType::Bytea, "{\"kind\":\"Bytea\"}"),
            (EndpointType::UUID, "{\"kind\":\"UUID\"}"),
            (EndpointType::NanoId { len: 0 }, "{\"kind\":\"NanoId\",\"len\":0}"),
            (EndpointType::NanoId { len: u8::MAX }, "{\"kind\":\"NanoId\",\"len\":255}"),
            (EndpointType::IpAddr, "{\"kind\":\"IpAddr\"}"),
            (EndpointType::Struct { name: String::from("Empty"), fields: Vec::new() },
                "{\"kind\":\"Struct\",\"name\":\"Empty\",\"fields\":[]}"),
            (EndpointType::StructRef(String::from("Unresolved")),
                "{\"kind\":\"StructRef\",\"value\":\"Unresolved\"}"),
            (EndpointType::Object, "{\"kind\":\"Object\"}"),
            (EndpointType::StructTable { struct_ref: String::from("Unresolved") },
                "{\"kind\":\"StructTable\",\"struct_ref\":\"Unresolved\"}"),
            (EndpointType::Vec(Box::new(EndpointType::Unit)),
                "{\"kind\":\"Vec\",\"value\":{\"kind\":\"Unit\"}}"),
            (EndpointType::Unit, "{\"kind\":\"Unit\"}"),
            (EndpointType::Optional(Box::new(EndpointType::UInt32)),
                "{\"kind\":\"Optional\",\"value\":{\"kind\":\"UInt32\"}}"),
            (EndpointType::Enum {
                name: String::from("State"),
                variants: vec![
                    EnumVariantSpec { name: String::from("Low"), value: i32::MIN, description: String::from("minimum") },
                    EnumVariantSpec { name: String::from("High"), value: i32::MAX, description: String::from("maximum") },
                ],
            }, concat!(
                "{\"kind\":\"Enum\",\"name\":\"State\",\"variants\":[",
                "{\"name\":\"Low\",\"value\":-2147483648,\"description\":\"minimum\"},",
                "{\"name\":\"High\",\"value\":2147483647,\"description\":\"maximum\"}]}"
            )),
            (EndpointType::Enum { name: String::from("EmptyEnum"), variants: Vec::new() },
                "{\"kind\":\"Enum\",\"name\":\"EmptyEnum\",\"variants\":[]}"),
            (EndpointType::EnumRef { name: String::from("State"), prefixed_name: false },
                "{\"kind\":\"EnumRef\",\"name\":\"State\",\"prefixed_name\":false}"),
            (EndpointType::EnumRef { name: String::from("State"), prefixed_name: true },
                "{\"kind\":\"EnumRef\",\"name\":\"State\",\"prefixed_name\":true}"),
            (EndpointType::TimeStampMs, "{\"kind\":\"TimeStampMs\"}"),
            (EndpointType::BlockchainDecimal, "{\"kind\":\"BlockchainDecimal\"}"),
            (EndpointType::BlockchainAddress, "{\"kind\":\"BlockchainAddress\"}"),
            (EndpointType::BlockchainTransactionHash, "{\"kind\":\"BlockchainTransactionHash\"}"),
        ]
    }

    #[test]
    fn public_selection_preserves_service_and_endpoint_order() {
        let input = catalog();
        let snapshot = input.clone();
        let selected = public_services(&input).unwrap();
        assert_eq!(selected, vec![
            ServiceSpec {
                id: input[0].id, name: input[0].name.clone(),
                endpoints: vec![input[0].endpoints[1].clone()],
            },
            input[2].clone(),
        ]);
        assert_eq!(input, snapshot);
        assert_eq!(selected, select_services(&input, Visibility::Public).unwrap());
    }

    #[test]
    fn all_selection_drops_only_empty_services() {
        let input = catalog();
        let expected = vec![input[0].clone(), input[2].clone(), input[3].clone()];
        assert_eq!(all_services(&input).unwrap(), expected);
        assert_eq!(select_services(&input, Visibility::All).unwrap(), expected);
    }

    #[test]
    fn roles_use_exact_any_role_intersection_and_always_include_public() {
        let input = catalog();
        let roles = [String::from("reader"), String::from("unused")];
        let selected = select_services(&input, Visibility::Roles(&roles)).unwrap();
        assert_eq!(selected[0].endpoints, input[0].endpoints[..2]);
        assert_eq!(selected[0].endpoints[0].roles, [String::from("reader"), String::from("admin")]);
        assert_eq!(selected[1], input[2]);
        assert_eq!(selected.len(), 2);

        for roles in [vec![], vec![String::from("Reader")], vec![String::from("read")], vec![String::new()]] {
            assert_eq!(
                select_services(&input, Visibility::Roles(&roles)).unwrap(),
                public_services(&input).unwrap(),
            );
        }
        let admin = [String::from("admin"), String::from("admin")];
        assert_eq!(select_services(&input, Visibility::Roles(&admin)).unwrap()[0], input[0]);
    }

    #[test]
    fn empty_catalog_and_no_visible_endpoints_return_empty_selections() {
        for visibility in [Visibility::All, Visibility::Public, Visibility::Roles(&[])] {
            assert!(select_services(&[], visibility).unwrap().is_empty());
        }
        let input = catalog();
        assert!(public_services(&input[3..]).unwrap().is_empty());
        assert_eq!(all_services(&input[1..2]).unwrap(), Vec::<ServiceSpec>::new());
    }

    #[test]
    fn selected_endpoint_metadata_is_independently_owned() {
        let input = catalog();
        let snapshot = input.clone();
        let mut selected = all_services(&input).unwrap();
        selected[0].name.clear();
        let copied = &mut selected[0].endpoints[0];
        copied.name.clear();
        copied.service.clear();
        copied.description.clear();
        copied.roles[0].clear();
        copied.params[0].name.clear();
        copied.params[0].description.clear();
        copied.response.clear();
        if let Some(EndpointType::Optional(child)) = &mut copied.stream_response {
            **child = EndpointType::Unit;
        } else {
            panic!("expected optional streaming type");
        }
        assert_eq!(input, snapshot);
        assert_ne!(selected[0].endpoints[0], input[0].endpoints[0]);
    }

    #[test]
    fn hidden_invalid_metadata_is_rejected_by_every_entry_point() {
        let mut input = catalog();
        input[3].endpoints[0].service = String::from("wrong");
        let expected = validate_services(&input).unwrap_err();
        assert_eq!(public_services(&input).unwrap_err(), expected);
        assert_eq!(all_services(&input).unwrap_err(), expected);
        assert_eq!(select_services(&input, Visibility::Roles(&[])).unwrap_err(), expected);
        let options = DocumentOptions { max_output_bytes: 0, max_type_depth: 0, ..DocumentOptions::default() };
        assert_eq!(services_document(&input, Visibility::Public, &options).unwrap_err(), expected);
    }

    #[test]
    fn validation_catches_global_service_and_endpoint_identity_collisions() {
        let base = catalog();
        let mut cases = Vec::new();
        let mut input = base.clone();
        input[3].id = input[0].id;
        cases.push(input);
        let mut input = base.clone();
        input[1].name = input[0].name.clone();
        cases.push(input);
        let mut input = base.clone();
        input[3].endpoints[0].code = input[0].endpoints[0].code;
        cases.push(input);
        let mut input = base.clone();
        input[0].endpoints[2].name = input[0].endpoints[0].name.clone();
        cases.push(input);
        for input in cases {
            assert_error(public_services(&input), ErrorCode::DuplicateObjectKey);
            assert_error(services_document(&input, Visibility::Public, &DocumentOptions::default()), ErrorCode::DuplicateObjectKey);
        }
    }

    #[test]
    fn validation_catches_bad_names_roles_and_fields_before_filtering() {
        for name in ["", "2bad", "a.b", "é", "has space"] {
            let mut input = catalog();
            input[1].name = String::from(name);
            assert_error(all_services(&input), ErrorCode::ConstraintViolation);
            let mut input = catalog();
            input[3].endpoints[0].name = String::from(name);
            assert_error(public_services(&input), ErrorCode::ConstraintViolation);
        }
        for roles in [vec![String::new()], vec![String::from("admin"), String::from("admin")]] {
            let mut input = catalog();
            input[3].endpoints[0].roles = roles;
            assert_error(public_services(&input), ErrorCode::ConstraintViolation);
        }
        for response in [false, true] {
            for empty in [false, true] {
                let mut input = catalog();
                let hidden = &mut input[3].endpoints[0];
                let fields = if response { &mut hidden.response } else { &mut hidden.params };
                if empty {
                    fields[0].name.clear();
                } else {
                    fields.push(fields[0].clone());
                }
                assert_error(public_services(&input), ErrorCode::DuplicateObjectKey);
            }
        }
    }

    #[test]
    fn full_document_has_exact_member_order_and_numeric_boundaries() {
        let input = vec![ServiceSpec {
            id: u16::MAX,
            name: String::from("api"),
            endpoints: vec![EndpointSchemaSpec {
                code: u32::MAX, name: String::from("ping"), service: String::from("api"),
                roles: Vec::new(),
                params: vec![EndpointField { name: String::from("x"), ty: EndpointType::UInt32, description: String::from("input") }],
                response: vec![EndpointField { name: String::from("y"), ty: EndpointType::Unit, description: String::from("output") }],
                stream_response: None, description: String::from("Ping."),
            }],
        }];
        let options = DocumentOptions {
            title: String::from("T"), version: String::from("V"), description: String::from("D"),
            server_url: Some(String::from("unused catalog value")),
            ..DocumentOptions::default()
        };
        let expected = concat!(
            "{\"title\":\"T\",\"version\":\"V\",\"description\":\"D\",\"services\":[",
            "{\"id\":65535,\"name\":\"api\",\"endpoints\":[",
            "{\"code\":4294967295,\"name\":\"ping\",\"service\":\"api\",\"roles\":[],",
            "\"params\":[{\"name\":\"x\",\"ty\":{\"kind\":\"UInt32\"},\"description\":\"input\"}],",
            "\"response\":[{\"name\":\"y\",\"ty\":{\"kind\":\"Unit\"},\"description\":\"output\"}],",
            "\"stream_response\":null,\"description\":\"Ping.\"}]}]}\n"
        );
        let document = services_document(&input, Visibility::Public, &options).unwrap();
        assert_eq!(document.media_type, "application/json");
        assert_eq!(document.text, expected);
        assert_eq!(services_document(&input, Visibility::All, &options).unwrap(), document);
    }

    #[test]
    fn empty_document_has_exact_shape_and_single_newline() {
        let document = services_document(&[], Visibility::All, &DocumentOptions::default()).unwrap();
        assert_eq!(document.text, "{\"title\":\"Endpoint API\",\"version\":\"1.0.0\",\"description\":\"\",\"services\":[]}\n");
        assert!(document.text.ends_with('\n'));
        assert!(!document.text.ends_with("\n\n"));
    }

    #[test]
    fn document_visibility_matches_selection_and_hides_all_endpoint_metadata() {
        let input = catalog();
        let options = DocumentOptions::default();
        let roles = [String::from("reader")];
        for visibility in [Visibility::All, Visibility::Public, Visibility::Roles(&roles)] {
            let selected = select_services(&input, visibility).unwrap();
            assert_eq!(
                services_document(&input, visibility, &options).unwrap(),
                services_document(&selected, Visibility::All, &options).unwrap(),
            );
        }
        let public = services_document(&input, Visibility::Public, &options).unwrap();
        for hidden in ["subscribe", "manage", "secret", "reader", "admin", "root", "private", "empty"] {
            assert!(!public.text.contains(hidden), "unexpected hidden value: {hidden}");
        }
        // Public fixture endpoints also have Optional stream types. Change
        // those before checking that hidden type descriptions cannot leak.
        let mut input = input;
        input[0].endpoints[1].stream_response = None;
        input[2].endpoints[0].stream_response = None;
        let public = services_document(&input, Visibility::Public, &options).unwrap();
        assert!(!public.text.contains("Optional"));
        assert!(public.text.contains("\"stream_response\":null"));
    }

    #[test]
    fn every_abi_type_has_exact_tag_member_order_and_lossless_clone() {
        for (ty, expected) in type_vectors() {
            assert_eq!(type_text(&ty, 2).unwrap(), expected);
            let copied = copy_type(&ty).unwrap();
            assert_eq!(copied, ty);
            assert_eq!(type_text(&copied, 2).unwrap(), expected);
        }
    }

    #[test]
    fn nested_struct_fields_and_enum_variants_keep_their_input_order() {
        let ty = EndpointType::Struct {
            name: String::from("Record"),
            fields: vec![
                EndpointField {
                    name: String::from("z"),
                    ty: EndpointType::Optional(Box::new(EndpointType::Vec(Box::new(
                        EndpointType::StructRef(String::from("Missing")),
                    )))),
                    description: String::from("nested"),
                },
                EndpointField { name: String::from("a"), ty: EndpointType::Unit, description: String::new() },
            ],
        };
        let expected = concat!(
            "{\"kind\":\"Struct\",\"name\":\"Record\",\"fields\":[",
            "{\"name\":\"z\",\"ty\":{\"kind\":\"Optional\",\"value\":{\"kind\":\"Vec\",",
            "\"value\":{\"kind\":\"StructRef\",\"value\":\"Missing\"}}},\"description\":\"nested\"},",
            "{\"name\":\"a\",\"ty\":{\"kind\":\"Unit\"},\"description\":\"\"}]}"
        );
        assert_eq!(type_text(&ty, 4).unwrap(), expected);
        assert_eq!(copy_type(&ty).unwrap(), ty);
        assert_error(type_text(&ty, 3), ErrorCode::DepthLimitExceeded);
    }

    #[test]
    fn named_references_are_serialized_without_resolution_or_depth_expansion() {
        for ty in [
            EndpointType::StructRef(String::from("Self")),
            EndpointType::StructTable { struct_ref: String::from("Self") },
            EndpointType::EnumRef { name: String::from("Missing"), prefixed_name: true },
        ] {
            let mut input = catalog();
            input[2].endpoints[0].stream_response = Some(ty.clone());
            let options = DocumentOptions { max_type_depth: 1, ..DocumentOptions::default() };
            let document = services_document(&input[2..3], Visibility::All, &options).unwrap();
            let encoded = type_text(&ty, 1).unwrap();
            assert!(document.text.contains(encoded.as_str()));
        }
    }

    #[test]
    fn json_strings_escape_every_control_byte_and_preserve_unicode() {
        let mut out = CatalogWriter::new(10_000);
        out.string("\"\\\u{0008}\u{000c}\n\r\t\0\u{0001}\u{001f}/é中😀\u{2028}\u{2029}").unwrap();
        assert_eq!(out.text, "\"\\\"\\\\\\b\\f\\n\\r\\t\\u0000\\u0001\\u001f/é中😀\u{2028}\u{2029}\"");
        for byte in 0u8..=31 {
            let input = [byte];
            let mut out = CatalogWriter::new(8);
            out.string(core::str::from_utf8(&input).unwrap()).unwrap();
            assert!(!out.text.bytes().any(|value| value < 32));
            assert!(out.text.starts_with('"') && out.text.ends_with('"'));
        }
        let mut out = CatalogWriter::new(100);
        out.string("").unwrap();
        out.string("é\n中\t😀\"end\\").unwrap();
        assert_eq!(out.text, "\"\"\"é\\n中\\t😀\\\"end\\\\\"");
    }

    #[test]
    fn untrusted_metadata_is_escaped_in_document_and_nested_type_positions() {
        let text = "\"\\\n\r\0é";
        let mut input = catalog();
        let endpoint = &mut input[2].endpoints[0];
        endpoint.description = String::from(text);
        endpoint.roles = vec![String::from(text)];
        endpoint.params[0].name = String::from(text);
        endpoint.params[0].description = String::from(text);
        endpoint.params[0].ty = EndpointType::Enum {
            name: String::from(text),
            variants: vec![EnumVariantSpec { name: String::from(text), value: -1, description: String::from(text) }],
        };
        endpoint.response[0].ty = EndpointType::Struct {
            name: String::from(text),
            fields: vec![EndpointField {
                name: String::from(text),
                ty: EndpointType::StructTable { struct_ref: String::from(text) },
                description: String::from(text),
            }],
        };
        let options = DocumentOptions {
            title: String::from(text), version: String::from(text), description: String::from(text),
            ..DocumentOptions::default()
        };
        let doc = services_document(&input[2..3], Visibility::All, &options).unwrap();
        let mut quoted = CatalogWriter::new(100);
        quoted.string(text).unwrap();
        assert_eq!(doc.text.matches(quoted.text.as_str()).count(), 14);
        assert!(!doc.text.contains('\0'));
        assert!(!doc.text.contains('\r'));
        assert_eq!(doc.text.bytes().filter(|byte| *byte == b'\n').count(), 1);
    }

    #[test]
    fn exact_output_limit_includes_newline_and_counts_utf8_bytes() {
        let options = DocumentOptions { title: String::from("é😀"), ..DocumentOptions::default() };
        let expected = services_document(&[], Visibility::All, &options).unwrap();
        let length = expected.text.len() as u32;
        assert!(expected.text.len() > expected.text.chars().count());
        for limit in [0, 1, length - 1] {
            let options = DocumentOptions { max_output_bytes: limit, ..options.clone() };
            assert_error(services_document(&[], Visibility::All, &options), ErrorCode::PayloadTooLarge);
        }
        for limit in [length, length + 1, u32::MAX] {
            let options = DocumentOptions { max_output_bytes: limit, ..options.clone() };
            assert_eq!(services_document(&[], Visibility::All, &options).unwrap(), expected);
        }
        let options = DocumentOptions { max_output_bytes: expected.text.chars().count() as u32, ..options };
        assert_error(services_document(&[], Visibility::All, &options), ErrorCode::PayloadTooLarge);
    }

    #[test]
    fn bounded_writer_rejects_before_appending_and_can_continue_after_error() {
        let mut out = CatalogWriter::new(3);
        out.raw("é").unwrap();
        let capacity = out.text.capacity();
        assert_error(out.raw("xy"), ErrorCode::PayloadTooLarge);
        assert_eq!(out.text, "é");
        assert_eq!(out.text.capacity(), capacity);
        out.raw("!").unwrap();
        out.raw("").unwrap();
        assert_error(out.raw("x"), ErrorCode::PayloadTooLarge);
        assert_eq!(out.text, "é!");
        let mut zero = CatalogWriter::new(0);
        zero.raw("").unwrap();
        assert_error(zero.string(""), ErrorCode::PayloadTooLarge);
        assert!(zero.text.is_empty());
    }

    #[test]
    fn every_type_encoding_obeys_its_exact_byte_limit() {
        for (ty, expected) in type_vectors() {
            let mut exact = CatalogWriter::new(expected.len() as u32);
            write_type(&mut exact, &ty, 2).unwrap();
            assert_eq!(exact.text, expected);
            let mut short = CatalogWriter::new(expected.len() as u32 - 1);
            assert_error(write_type(&mut short, &ty, 2), ErrorCode::PayloadTooLarge);
            assert!(short.text.len() < expected.len());
        }
    }

    #[test]
    fn inline_depth_limit_counts_types_not_fields_or_enum_entries() {
        assert_eq!(type_text(&EndpointType::Unit, 1).unwrap(), "{\"kind\":\"Unit\"}");
        let ty = EndpointType::Optional(Box::new(EndpointType::Vec(Box::new(EndpointType::Unit))));
        assert!(type_text(&ty, 3).is_ok());
        assert_error(type_text(&ty, 2), ErrorCode::DepthLimitExceeded);
        let empty = EndpointType::Struct { name: String::from("Empty"), fields: Vec::new() };
        assert!(type_text(&empty, 1).is_ok());
        let fields = EndpointType::Struct {
            name: String::from("Pair"),
            fields: vec![field("second", EndpointType::Unit), field("first", EndpointType::Int64)],
        };
        assert_error(type_text(&fields, 1), ErrorCode::DepthLimitExceeded);
        assert!(type_text(&fields, 2).is_ok());
        for (ty, _) in type_vectors() {
            if matches!(&ty, EndpointType::Enum { .. }) {
                assert!(type_text(&ty, 1).is_ok());
            }
        }
    }

    #[test]
    fn zero_depth_is_invalid_even_for_empty_catalogs() {
        let options = DocumentOptions { max_type_depth: 0, ..DocumentOptions::default() };
        assert_error(services_document(&[], Visibility::Public, &options), ErrorCode::ConstraintViolation);
        for ty in [EndpointType::Vec(Box::new(EndpointType::Unit)), EndpointType::Optional(Box::new(EndpointType::Unit))] {
            let mut input = catalog();
            input[2].endpoints[0].stream_response = Some(ty);
            let options = DocumentOptions { max_type_depth: 1, ..DocumentOptions::default() };
            assert_error(services_document(&input[2..3], Visibility::Public, &options), ErrorCode::DepthLimitExceeded);
        }
    }

    #[test]
    fn max_u16_depth_does_not_wrap_when_scheduling_inline_children() {
        let ty = EndpointType::Optional(Box::new(EndpointType::Unit));
        let mut out = CatalogWriter::new(1000);
        let mut tasks = vec![TypeTask::Type(&ty, u32::from(u16::MAX))];
        assert_error(write_tasks(&mut out, &mut tasks, u16::MAX), ErrorCode::DepthLimitExceeded);
        let options = DocumentOptions { max_type_depth: u16::MAX, ..DocumentOptions::default() };
        assert!(services_document(&[], Visibility::All, &options).is_ok());
    }

    #[test]
    fn deep_type_copy_and_serialization_use_explicit_traversal_stacks() {
        let mut ty = EndpointType::Struct {
            name: String::from("Leaf"),
            fields: vec![field("a", EndpointType::Unit), field("b", EndpointType::Int64)],
        };
        for _ in 0..256 {
            ty = EndpointType::Optional(Box::new(ty));
        }
        let copied = copy_type(&ty).unwrap();
        assert_eq!(copied, ty);
        let text = type_text(&copied, 258).unwrap();
        assert_eq!(text.matches("\"kind\":\"Optional\"").count(), 256);
        assert_error(type_text(&copied, 257), ErrorCode::DepthLimitExceeded);
    }

    #[test]
    fn integer_text_is_exact_including_signed_extremes() {
        for (value, expected) in [
            (i64::MIN, "-9223372036854775808"),
            (i64::MAX, "9223372036854775807"),
            (i64::from(i32::MIN), "-2147483648"),
            (i64::from(u32::MAX), "4294967295"),
            (-1, "-1"), (0, "0"), (1, "1"), (10, "10"),
        ] {
            let mut out = CatalogWriter::new(expected.len() as u32);
            out.integer(value).unwrap();
            assert_eq!(out.text, expected);
        }
    }

    #[test]
    fn allocation_capacity_overflow_uses_canonical_error() {
        assert_error(reserved_vec::<u8>(usize::MAX), ErrorCode::BudgetExhausted);
        assert!(reserved_vec::<EndpointType>(0).unwrap().is_empty());
        let boxed = boxed_type(EndpointType::Unit).unwrap();
        assert_eq!(*boxed, EndpointType::Unit);
    }

    #[test]
    fn stable_inputs_produce_identical_bytes_without_mutation() {
        let input = catalog();
        let snapshot = input.clone();
        let options = DocumentOptions::default();
        let expected = services_document(&input, Visibility::All, &options).unwrap();
        for _ in 0..4 {
            assert_eq!(services_document(&input, Visibility::All, &options).unwrap(), expected);
        }
        assert_eq!(input, snapshot);
    }
}
