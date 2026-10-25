use super as common_types;
use super::R;
use super::ValueKind;
use jmax_native::{NativeContext, NativeDecode, NativeResult, Reader, VmLimits};
use jmax_schema_ir::{
    ArrayShape, ObjectShape, Property, ScalarConstraints, Schema, TypeId, TypeNode,
};

#[derive(Debug, PartialEq)]
pub enum NativeProjection {
    Twitter(common_types::Twitter),
    Citm(common_types::Citm),
    Canada(common_types::Canada),
    Github(Vec<common_types::Github>),
    Apache(common_types::Apache),
    TypedPayload(common_types::TypedPayload),
}
pub struct NativeScratch {
    context: NativeContext,
}

impl Default for NativeScratch {
    fn default() -> Self {
        let options = common_types::options();
        Self {
            context: NativeContext::new(
                VmLimits {
                    syntax: options.limits,
                    max_steps: jmax_schema_vm::DEFAULT_MAX_VM_STEPS,
                    max_scratch_bytes: 16 * 1024 * 1024,
                },
                options.numeric,
            ),
        }
    }
}

pub fn project(name: &str, raw: &[u8], scratch: &mut NativeScratch) -> R<NativeProjection> {
    macro_rules! decode {
        ($ty:ty, $variant:ident) => {{
            let mut value = <$ty>::default();
            jmax_native::decode_into(&mut scratch.context, raw, &mut value)?;
            Ok(NativeProjection::$variant(value.into()))
        }};
    }
    match name {
        "twitter" => decode!(TwitterNative, Twitter),
        "citm_catalog" => decode!(CitmNative, Citm),
        "canada" => decode!(CanadaNative, Canada),
        "github_events" => decode!(GithubNative, Github),
        "apache_builds" => decode!(ApacheNative, Apache),
        "typed_payload" | "endpoint_batch" => decode!(TypedPayloadNative, TypedPayload),
        _ => Err(format!("unknown native projection {name}").into()),
    }
}

#[derive(Default)]
struct TwitterNative {
    query: String,
    count: i64,
    max_id: i64,
}
#[derive(Default)]
struct CitmNative(String);
#[derive(Default)]
struct CanadaNative(String);
#[derive(Default)]
struct GithubNative(Vec<common_types::Github>);
#[derive(Default)]
struct ApacheNative {
    mode: String,
    node_name: String,
    executors: i64,
    quieting_down: bool,
}
#[derive(Default)]
struct TypedPayloadNative {
    batch_id: String,
    tenant: String,
    schema_version: i64,
    request_count: i64,
    accepted: bool,
}

impl From<TwitterNative> for common_types::Twitter {
    fn from(value: TwitterNative) -> Self {
        Self {
            search_metadata: common_types::SearchMetadata {
                query: value.query,
                count: value.count,
                max_id: value.max_id,
            },
        }
    }
}
impl From<CitmNative> for common_types::Citm {
    fn from(value: CitmNative) -> Self {
        Self {
            venueNames: common_types::Venues {
                PLEYEL_PLEYEL: value.0,
            },
        }
    }
}
impl From<CanadaNative> for common_types::Canada {
    fn from(value: CanadaNative) -> Self {
        Self { r#type: value.0 }
    }
}
impl From<GithubNative> for Vec<common_types::Github> {
    fn from(value: GithubNative) -> Self {
        value.0
    }
}
impl From<ApacheNative> for common_types::Apache {
    fn from(value: ApacheNative) -> Self {
        Self {
            mode: value.mode,
            nodeName: value.node_name,
            numExecutors: value.executors,
            quietingDown: value.quieting_down,
        }
    }
}
impl From<TypedPayloadNative> for common_types::TypedPayload {
    fn from(value: TypedPayloadNative) -> Self {
        Self {
            batch_id: value.batch_id,
            tenant: value.tenant,
            schema_version: value.schema_version,
            request_count: value.request_count,
            accepted: value.accepted,
        }
    }
}

fn aot_program(
    name: &str,
    fields: &[(&[&str], ValueKind)],
    array_root: bool,
) -> jmax_native::Result<jmax_native::ValidatedProgram> {
    let object_root = if array_root { 1usize } else { 0usize };
    let mut nodes = Vec::new();
    if array_root {
        nodes.push(TypeNode::Array(ArrayShape {
            nullable: false,
            items: TypeId(1),
            min_items: None,
            max_items: None,
        }));
    }
    nodes.push(TypeNode::Object(ObjectShape {
        nullable: false,
        properties: Vec::new(),
        reject_unknown: false,
        min_properties: None,
        max_properties: None,
    }));
    for (slot, (path, kind)) in fields.iter().enumerate() {
        let mut parent = object_root;
        for (depth, key) in path.iter().enumerate() {
            let last = depth + 1 == path.len();
            let existing = match &nodes[parent] {
                TypeNode::Object(object) => object
                    .properties
                    .iter()
                    .find(|property| property.name == *key)
                    .map(|property| property.ty.0 as usize),
                _ => None,
            };
            if let Some(child) = existing {
                parent = child;
                continue;
            }
            let child = nodes.len();
            nodes.push(if last {
                TypeNode::Scalar(ScalarConstraints::new(*kind))
            } else {
                TypeNode::Object(ObjectShape {
                    nullable: false,
                    properties: Vec::new(),
                    reject_unknown: false,
                    min_properties: None,
                    max_properties: None,
                })
            });
            if let TypeNode::Object(object) = &mut nodes[parent] {
                object.properties.push(Property {
                    field_id: jmax_abi::FieldId(if last {
                        slot as u32 + 1
                    } else {
                        1000 + child as u32
                    }),
                    name: (*key).into(),
                    ty: TypeId(child as u32),
                    required: true,
                    slot: if last && !array_root {
                        Some(slot as u32)
                    } else {
                        None
                    },
                });
            }
            parent = child;
        }
    }
    jmax_schema_aot::compile_graph(&Schema {
        name: name.into(),
        schema_id: jmax_abi::SchemaId(1),
        root: TypeId(0),
        nodes,
        definitions: Vec::new(),
    })
}

macro_rules! object_start {
    ($reader:ident, $ctx:ident, $depth:expr) => {{
        if $depth > $ctx.limits().syntax.max_depth {
            return Err(jmax_native::Fail);
        }
        $reader.expect(b'{')?;
        ($ctx.begin_object(), 0usize)
    }};
}

macro_rules! unknown_field {
    ($reader:ident, $ctx:ident, $depth:expr) => {{
        if $ctx.limits().syntax.reject_unknown_keys {
            return Err(jmax_native::Fail);
        }
        $reader.unknown_key($ctx)?;
        $reader.skip_value($ctx, $depth)?;
    }};
}

// Probe the next field in the corpus' usual layout with the raw-key matcher.
// Other orders, whitespace before a colon, and escaped key spellings retain
// the decoded matcher as the compatibility path.
macro_rules! predicted_key {
    ($reader:ident, $ctx:ident, $next:ident, $unknown:expr; $($index:literal => $pattern:expr),+ $(,)?) => {{
        let predicted = $next;
        let mut matched = None;
        $(
            if predicted == $index && $reader.match_key($ctx, $pattern) {
                matched = Some($index);
            }
        )+
        if matched.is_none() {
            $(
                if matched.is_none() && predicted != $index && $reader.match_key_decoded($ctx, $pattern) {
                    matched = Some($index);
                }
            )+
        }
        if matched.is_none() {
            $(
                if predicted == $index && $reader.match_key_decoded($ctx, $pattern) {
                    matched = Some($index);
                }
            )+
        }
        matched.unwrap_or($unknown)
    }};
}

impl NativeDecode for TwitterNative {
    const COUNTS_NATIVE_BUDGET: bool = true;
    const COUNTS_ENUM_VALUES: bool = true;
    fn build_program() -> jmax_native::Result<jmax_native::ValidatedProgram> {
        aot_program(
            "twitter",
            &[
                (&["search_metadata", "query"], ValueKind::String),
                (&["search_metadata", "count"], ValueKind::Integer),
                (&["search_metadata", "max_id"], ValueKind::Integer),
            ],
            false,
        )
    }
    fn decode_core(
        r: &mut Reader<'_>,
        ctx: &mut NativeContext,
        out: &mut Self,
    ) -> NativeResult<()> {
        let (mark, mut fields) = object_start!(r, ctx, 1);
        let mut seen_metadata = false;
        let next = 0usize;
        if !r.eat(b'}') {
            loop {
                fields += 1;
                if fields > ctx.limits().syntax.max_fields_per_object as usize {
                    return Err(jmax_native::Fail);
                }
                if predicted_key!(r, ctx, next, 1; 0 => b"\"search_metadata\":") == 0 {
                    if seen_metadata && ctx.limits().syntax.reject_duplicate_keys {
                        return Err(jmax_native::Fail);
                    }
                    seen_metadata = true;
                    decode_metadata(r, ctx, out)?;
                } else {
                    unknown_field!(r, ctx, 1);
                }
                if r.eat(b',') {
                    continue;
                }
                r.expect(b'}')?;
                break;
            }
        }
        if !seen_metadata {
            return Err(jmax_native::Fail);
        }
        ctx.end_object(mark)
    }
}

fn decode_metadata(
    r: &mut Reader<'_>,
    ctx: &mut NativeContext,
    out: &mut TwitterNative,
) -> NativeResult<()> {
    let (mark, mut fields) = object_start!(r, ctx, 2);
    let mut seen = [false; 3];
    let mut next = 2usize;
    if !r.eat(b'}') {
        loop {
            fields += 1;
            if fields > ctx.limits().syntax.max_fields_per_object as usize {
                return Err(jmax_native::Fail);
            }
            let index = predicted_key!(r, ctx, next, 3;
                0 => b"\"query\":",
                1 => b"\"count\":",
                2 => b"\"max_id\":",
            );
            if index < 3 {
                next = (index + 1) % 3;
                if seen[index] && ctx.limits().syntax.reject_duplicate_keys {
                    return Err(jmax_native::Fail);
                }
                seen[index] = true;
                match index {
                    0 => out.query = r.string(ctx)?.to_owned(),
                    1 => out.count = r.int(ctx, i64::MIN, i64::MAX)?,
                    _ => out.max_id = r.int(ctx, i64::MIN, i64::MAX)?,
                }
            } else {
                unknown_field!(r, ctx, 2);
            }
            if r.eat(b',') {
                continue;
            }
            r.expect(b'}')?;
            break;
        }
    }
    if seen.iter().any(|present| !present) {
        return Err(jmax_native::Fail);
    }
    ctx.end_object(mark)
}

fn decode_string_object(
    r: &mut Reader<'_>,
    ctx: &mut NativeContext,
    key_name: &[u8],
    depth: u16,
) -> NativeResult<String> {
    let (mark, mut fields) = object_start!(r, ctx, depth);
    let mut seen = false;
    let next = 0usize;
    let mut value = String::new();
    if !r.eat(b'}') {
        loop {
            fields += 1;
            if fields > ctx.limits().syntax.max_fields_per_object as usize {
                return Err(jmax_native::Fail);
            }
            let matched = if key_name == b"PLEYEL_PLEYEL" {
                predicted_key!(r, ctx, next, 1; 0 => b"\"PLEYEL_PLEYEL\":") == 0
            } else {
                false
            };
            if matched {
                if seen && ctx.limits().syntax.reject_duplicate_keys {
                    return Err(jmax_native::Fail);
                }
                seen = true;
                value = r.string(ctx)?.to_owned();
            } else {
                unknown_field!(r, ctx, depth);
            }
            if r.eat(b',') {
                continue;
            }
            r.expect(b'}')?;
            break;
        }
    }
    if !seen {
        return Err(jmax_native::Fail);
    }
    ctx.end_object(mark)?;
    Ok(value)
}

impl NativeDecode for CitmNative {
    const COUNTS_NATIVE_BUDGET: bool = true;
    const COUNTS_ENUM_VALUES: bool = true;
    fn build_program() -> jmax_native::Result<jmax_native::ValidatedProgram> {
        aot_program(
            "citm_catalog",
            &[(&["venueNames", "PLEYEL_PLEYEL"], ValueKind::String)],
            false,
        )
    }
    fn decode_core(
        r: &mut Reader<'_>,
        ctx: &mut NativeContext,
        out: &mut Self,
    ) -> NativeResult<()> {
        let (mark, mut fields) = object_start!(r, ctx, 1);
        let mut seen = false;
        let next = 0usize;
        if !r.eat(b'}') {
            loop {
                fields += 1;
                if fields > ctx.limits().syntax.max_fields_per_object as usize {
                    return Err(jmax_native::Fail);
                }
                if predicted_key!(r, ctx, next, 1; 0 => b"\"venueNames\":") == 0 {
                    if seen && ctx.limits().syntax.reject_duplicate_keys {
                        return Err(jmax_native::Fail);
                    }
                    seen = true;
                    out.0 = decode_string_object(r, ctx, b"PLEYEL_PLEYEL", 2)?;
                } else {
                    unknown_field!(r, ctx, 1);
                }
                if r.eat(b',') {
                    continue;
                }
                r.expect(b'}')?;
                break;
            }
        }
        if !seen {
            return Err(jmax_native::Fail);
        }
        ctx.end_object(mark)
    }
}

impl NativeDecode for CanadaNative {
    const COUNTS_NATIVE_BUDGET: bool = true;
    const COUNTS_ENUM_VALUES: bool = true;
    fn build_program() -> jmax_native::Result<jmax_native::ValidatedProgram> {
        aot_program("canada", &[(&["type"], ValueKind::String)], false)
    }
    fn decode_core(
        r: &mut Reader<'_>,
        ctx: &mut NativeContext,
        out: &mut Self,
    ) -> NativeResult<()> {
        let (mark, mut fields) = object_start!(r, ctx, 1);
        let mut seen = false;
        let next = 0usize;
        if !r.eat(b'}') {
            loop {
                fields += 1;
                if fields > ctx.limits().syntax.max_fields_per_object as usize {
                    return Err(jmax_native::Fail);
                }
                if predicted_key!(r, ctx, next, 1; 0 => b"\"type\":") == 0 {
                    if seen && ctx.limits().syntax.reject_duplicate_keys {
                        return Err(jmax_native::Fail);
                    }
                    seen = true;
                    out.0 = r.string(ctx)?.to_owned();
                } else {
                    unknown_field!(r, ctx, 1);
                }
                if r.eat(b',') {
                    continue;
                }
                r.expect(b'}')?;
                break;
            }
        }
        if !seen {
            return Err(jmax_native::Fail);
        }
        ctx.end_object(mark)
    }
}

fn decode_github_row(
    r: &mut Reader<'_>,
    ctx: &mut NativeContext,
    depth: u16,
) -> NativeResult<common_types::Github> {
    let (mark, mut fields) = object_start!(r, ctx, depth);
    let mut seen = [false; 3];
    let mut next = 0usize;
    let mut row = common_types::Github {
        id: String::new(),
        r#type: String::new(),
        public: false,
    };
    if !r.eat(b'}') {
        loop {
            fields += 1;
            if fields > ctx.limits().syntax.max_fields_per_object as usize {
                return Err(jmax_native::Fail);
            }
            let index = predicted_key!(r, ctx, next, 3;
                0 => b"\"id\":",
                1 => b"\"type\":",
                2 => b"\"public\":",
            );
            if index < 3 {
                next = (index + 1) % 3;
                if seen[index] && ctx.limits().syntax.reject_duplicate_keys {
                    return Err(jmax_native::Fail);
                }
                seen[index] = true;
                match index {
                    0 => row.id = r.string(ctx)?.to_owned(),
                    1 => row.r#type = r.string(ctx)?.to_owned(),
                    _ => row.public = r.boolean()?,
                }
            } else {
                unknown_field!(r, ctx, depth);
            }
            if r.eat(b',') {
                continue;
            }
            r.expect(b'}')?;
            break;
        }
    }
    if seen.iter().any(|present| !present) {
        return Err(jmax_native::Fail);
    }
    ctx.end_object(mark)?;
    Ok(row)
}

impl NativeDecode for GithubNative {
    const COUNTS_NATIVE_BUDGET: bool = true;
    const COUNTS_ENUM_VALUES: bool = true;
    fn build_program() -> jmax_native::Result<jmax_native::ValidatedProgram> {
        aot_program(
            "github_events",
            &[
                (&["id"], ValueKind::String),
                (&["type"], ValueKind::String),
                (&["public"], ValueKind::Boolean),
            ],
            true,
        )
    }
    fn decode_core(
        r: &mut Reader<'_>,
        ctx: &mut NativeContext,
        out: &mut Self,
    ) -> NativeResult<()> {
        if 1 > ctx.limits().syntax.max_depth {
            return Err(jmax_native::Fail);
        }
        r.expect(b'[')?;
        let mut items = 0usize;
        out.0.reserve(32);
        if !r.eat(b']') {
            loop {
                items += 1;
                if items > ctx.limits().syntax.max_array_items as usize {
                    return Err(jmax_native::Fail);
                }
                out.0.push(decode_github_row(r, ctx, 2)?);
                if r.eat(b',') {
                    continue;
                }
                r.expect(b']')?;
                break;
            }
        }
        Ok(())
    }
}

impl NativeDecode for ApacheNative {
    const COUNTS_NATIVE_BUDGET: bool = true;
    const COUNTS_ENUM_VALUES: bool = true;
    fn build_program() -> jmax_native::Result<jmax_native::ValidatedProgram> {
        aot_program(
            "apache_builds",
            &[
                (&["mode"], ValueKind::String),
                (&["nodeName"], ValueKind::String),
                (&["numExecutors"], ValueKind::Integer),
                (&["quietingDown"], ValueKind::Boolean),
            ],
            false,
        )
    }
    fn decode_core(
        r: &mut Reader<'_>,
        ctx: &mut NativeContext,
        out: &mut Self,
    ) -> NativeResult<()> {
        let (mark, mut fields) = object_start!(r, ctx, 1);
        let mut seen = [false; 4];
        let mut next = 0usize;
        if !r.eat(b'}') {
            loop {
                fields += 1;
                if fields > ctx.limits().syntax.max_fields_per_object as usize {
                    return Err(jmax_native::Fail);
                }
                let index = predicted_key!(r, ctx, next, 4;
                    0 => b"\"mode\":",
                    1 => b"\"nodeName\":",
                    2 => b"\"numExecutors\":",
                    3 => b"\"quietingDown\":",
                );
                if index < 4 {
                    next = (index + 1) % 4;
                    if seen[index] && ctx.limits().syntax.reject_duplicate_keys {
                        return Err(jmax_native::Fail);
                    }
                    seen[index] = true;
                    match index {
                        0 => out.mode = r.string(ctx)?.to_owned(),
                        1 => out.node_name = r.string(ctx)?.to_owned(),
                        2 => out.executors = r.int(ctx, i64::MIN, i64::MAX)?,
                        _ => out.quieting_down = r.boolean()?,
                    }
                } else {
                    unknown_field!(r, ctx, 1);
                }
                if r.eat(b',') {
                    continue;
                }
                r.expect(b'}')?;
                break;
            }
        }
        if seen.iter().any(|present| !present) {
            return Err(jmax_native::Fail);
        }
        ctx.end_object(mark)
    }
}

impl NativeDecode for TypedPayloadNative {
    const COUNTS_NATIVE_BUDGET: bool = true;
    const COUNTS_ENUM_VALUES: bool = true;
    fn build_program() -> jmax_native::Result<jmax_native::ValidatedProgram> {
        aot_program(
            "typed_payload",
            &[
                (&["batch_id"], ValueKind::String),
                (&["tenant"], ValueKind::String),
                (&["schema_version"], ValueKind::Integer),
                (&["request_count"], ValueKind::Integer),
                (&["accepted"], ValueKind::Boolean),
            ],
            false,
        )
    }
    fn decode_core(
        r: &mut Reader<'_>,
        ctx: &mut NativeContext,
        out: &mut Self,
    ) -> NativeResult<()> {
        let (mark, mut fields) = object_start!(r, ctx, 1);
        let mut seen = [false; 5];
        let mut next = 0usize;
        if !r.eat(b'}') {
            loop {
                fields += 1;
                if fields > ctx.limits().syntax.max_fields_per_object as usize {
                    return Err(jmax_native::Fail);
                }
                let index = predicted_key!(r, ctx, next, 5;
                    0 => b"\"batch_id\":",
                    1 => b"\"tenant\":",
                    2 => b"\"schema_version\":",
                    3 => b"\"request_count\":",
                    4 => b"\"accepted\":",
                );
                if index < 5 {
                    next = (index + 1) % 5;
                    if seen[index] && ctx.limits().syntax.reject_duplicate_keys {
                        return Err(jmax_native::Fail);
                    }
                    seen[index] = true;
                    match index {
                        0 => out.batch_id = r.string(ctx)?.to_owned(),
                        1 => out.tenant = r.string(ctx)?.to_owned(),
                        2 => out.schema_version = r.int(ctx, i64::MIN, i64::MAX)?,
                        3 => out.request_count = r.int(ctx, i64::MIN, i64::MAX)?,
                        _ => out.accepted = r.boolean()?,
                    }
                } else {
                    unknown_field!(r, ctx, 1);
                }
                if r.eat(b',') {
                    continue;
                }
                r.expect(b'}')?;
                break;
            }
        }
        if seen.iter().any(|present| !present) {
            return Err(jmax_native::Fail);
        }
        ctx.end_object(mark)
    }
}

/// Verify the selected native fields against the same serde projection.
pub fn check(name: &str, raw: &[u8]) -> R<()> {
    let got = project(name, raw, &mut NativeScratch::default())?;
    let want = match name {
        "twitter" => NativeProjection::Twitter(serde_json::from_slice(raw)?),
        "citm_catalog" => NativeProjection::Citm(serde_json::from_slice(raw)?),
        "canada" => NativeProjection::Canada(serde_json::from_slice(raw)?),
        "github_events" => NativeProjection::Github(serde_json::from_slice(raw)?),
        "apache_builds" => NativeProjection::Apache(serde_json::from_slice(raw)?),
        _ => NativeProjection::TypedPayload(serde_json::from_slice(raw)?),
    };
    assert_eq!(got, want, "native projection {name}");
    Ok(())
}
