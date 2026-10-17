//! Deterministic, bounded benchmark workload generation.

use alloc::{string::String, vec::Vec};
use core::convert::TryFrom;

use jmax_abi::{
    error::ErrorCode,
    schema::{FieldId, SchemaFieldIr, SchemaId, SchemaIr, Selection},
    syntax::ScalarKind,
};

use crate::{CorpusConfig, DeterministicRng, Payload, Result, Workload, WorkloadKind, bench_error};

const MAX_CONFIG_COUNT: u32 = 1_000_000;
const MAX_NESTING: u32 = 64;
const STRICT_PAYLOAD_LIMIT: usize = 16 * 1024 * 1024;
const SMALL_OBJECT_SCHEMA_ID: u32 = 0x4a4d_0001;
const ESCAPED_STRINGS_SCHEMA_ID: u32 = 0x4a4d_0002;
const UNICODE_SCHEMA_ID: u32 = 0x4a4d_0003;

/// Build one reproducible corpus of the requested workload kind.
pub fn make_workload(kind: WorkloadKind, config: CorpusConfig) -> Result<Workload> {
    validate_config(config)?;
    if crate::followup::is_followup(kind) {
        return crate::followup::make_workload(kind, config);
    }
    validate_kind_config(kind, config)?;
    build_workload(kind, config, config.max_bytes)
}

/// Build all standard workload kinds in the contract's stable order.
pub fn make_suite(config: CorpusConfig) -> Result<Vec<Workload>> {
    validate_config(config)?;
    let kinds = [
        WorkloadKind::SmallObject,
        WorkloadKind::EscapedStrings,
        WorkloadKind::Unicode,
        WorkloadKind::NumberArray,
        WorkloadKind::DepthBook,
        WorkloadKind::AggTrade,
        WorkloadKind::Nested,
    ];
    let mut minimum_total = 0u64;
    for kind in kinds {
        validate_kind_config(kind, config)?;
        minimum_total = minimum_total
            .checked_add(minimum_workload_bytes(kind, config)?)
            .ok_or_else(|| budget_error("minimum suite byte count overflow"))?;
    }
    if minimum_total > config.max_bytes {
        return Err(budget_error("byte budget cannot hold minimum suite"));
    }
    let mut suite = Vec::new();
    suite
        .try_reserve_exact(kinds.len())
        .map_err(|_| budget_error("suite allocation failed"))?;
    let mut accumulated = 0u64;
    for kind in kinds {
        let remaining = config
            .max_bytes
            .checked_sub(accumulated)
            .ok_or_else(|| budget_error("suite byte budget underflow"))?;
        if remaining == 0 {
            return Err(budget_error("aggregate suite byte budget exceeded"));
        }
        let workload = build_workload(kind, config, remaining)?;
        accumulated = accumulated
            .checked_add(workload.total_bytes()?)
            .ok_or_else(|| budget_error("aggregate suite byte count overflow"))?;
        if accumulated > config.max_bytes {
            return Err(budget_error("aggregate suite byte budget exceeded"));
        }
        suite.push(workload);
    }
    Ok(suite)
}

/// Validate a caller-owned workload, including all bytes and schema metadata.
pub fn validate_workload(workload: &Workload, max_bytes: u64) -> Result<()> {
    if workload.name.is_empty() || workload.payloads.is_empty() || max_bytes == 0 {
        return Err(constraint_error(
            "workload name, corpus, and byte budget must be nonempty",
        ));
    }
    let total = workload.total_bytes()?;
    if total > max_bytes {
        return Err(budget_error("workload exceeds byte budget"));
    }
    for payload in &workload.payloads {
        let len = u64::try_from(payload.len())
            .map_err(|_| bench_error(ErrorCode::PayloadTooLarge, "payload length exceeds u64"))?;
        if len == 0 {
            return Err(bench_error(
                ErrorCode::UnexpectedEof,
                "empty workload payload",
            ));
        }
        if len > u64::from(u32::MAX) || payload.len() > STRICT_PAYLOAD_LIMIT {
            return Err(bench_error(
                ErrorCode::PayloadTooLarge,
                "payload exceeds strict ABI limits",
            ));
        }
        jsonmax::parse::validate_syntax(payload.as_slice(), jsonmax::ParseOptions::default())?;
    }

    let compiled = &workload.compiled_schema;
    if let Some(image) = &workload.schema {
        let retained = compiled.image();
        if image.header != retained.header
            || image.insns != retained.insns
            || image.keys != retained.keys
            || image.key_bytes != retained.key_bytes
        {
            return Err(bench_error(
                ErrorCode::SchemaImageInvalid,
                "workload image differs from its compiled schema",
            ));
        }
    }
    jmax_schema_vm::check_selection(compiled.program(), &workload.selection)?;

    Ok(())
}

fn validate_config(config: CorpusConfig) -> Result<()> {
    if config.documents == 0
        || config.items_per_document == 0
        || config.max_bytes == 0
        || config.documents > MAX_CONFIG_COUNT
        || config.items_per_document > MAX_CONFIG_COUNT
    {
        return Err(constraint_error("invalid corpus dimensions or byte budget"));
    }
    let minimum_per_document = match config_kind_minimum(config.items_per_document) {
        Some(value) => value,
        None => return Err(budget_error("minimum corpus size arithmetic overflow")),
    };
    let minimum = minimum_per_document
        .checked_mul(u64::from(config.documents))
        .ok_or_else(|| budget_error("minimum corpus size arithmetic overflow"))?;
    if minimum > config.max_bytes {
        return Err(budget_error("byte budget cannot hold minimum corpus"));
    }
    Ok(())
}

fn validate_kind_config(kind: WorkloadKind, config: CorpusConfig) -> Result<()> {
    if matches!(
        kind,
        WorkloadKind::SmallObject | WorkloadKind::EscapedStrings | WorkloadKind::Unicode
    ) && config.items_per_document > 65_536
    {
        return Err(constraint_error(
            "object field count exceeds strict parser limits",
        ));
    }
    if matches!(kind, WorkloadKind::DepthBook)
        && config.items_per_document > jmax_exchange::ExchangeProfile::default().max_levels
    {
        return Err(constraint_error(
            "depth level count exceeds the exchange profile limit",
        ));
    }
    Ok(())
}

fn config_kind_minimum(items: u32) -> Option<u64> {
    // Every generated value has at least two bytes; object forms have at least
    // one key/value pair and array forms at least one element.
    let repeated = u64::from(items);
    1u64.checked_add(repeated.checked_mul(2)?)
}

fn minimum_workload_bytes(kind: WorkloadKind, config: CorpusConfig) -> Result<u64> {
    let items = u64::from(config.items_per_document);
    let per_document = match kind {
        WorkloadKind::SmallObject | WorkloadKind::EscapedStrings | WorkloadKind::Unicode => 2u64
            .checked_add(
                items
                    .checked_mul(11)
                    .ok_or_else(|| budget_error("minimum payload size overflow"))?,
            ),
        WorkloadKind::NumberArray => 1u64.checked_add(
            items
                .checked_mul(2)
                .ok_or_else(|| budget_error("minimum payload size overflow"))?,
        ),
        WorkloadKind::DepthBook => 50u64.checked_add(
            items
                .checked_mul(12)
                .and_then(|value| value.checked_mul(2))
                .ok_or_else(|| budget_error("minimum payload size overflow"))?,
        ),
        WorkloadKind::AggTrade => Some(120),
        WorkloadKind::Nested => {
            Some(u64::from((config.items_per_document % MAX_NESTING).max(1)) * 2 + 1)
        }
        _ => return Err(constraint_error("use followup suite for this workload")),
    }
    .ok_or_else(|| budget_error("minimum payload size overflow"))?;
    per_document
        .checked_mul(u64::from(config.documents))
        .ok_or_else(|| budget_error("minimum corpus size overflow"))
}

fn build_workload(kind: WorkloadKind, config: CorpusConfig, budget: u64) -> Result<Workload> {
    if budget == 0 {
        return Err(budget_error("byte budget cannot hold workload"));
    }
    if minimum_workload_bytes(kind, config)? > budget {
        return Err(budget_error("byte budget cannot hold minimum workload"));
    }
    let name = workload_name(kind, config)?;
    let mut payloads = Vec::new();
    payloads
        .try_reserve_exact(config.documents as usize)
        .map_err(|_| budget_error("payload table allocation failed"))?;
    let mut used = 0u64;
    let mut rng = DeterministicRng::new(config.seed);
    for document_index in 0..config.documents {
        let remaining = budget
            .checked_sub(used)
            .ok_or_else(|| budget_error("corpus byte budget underflow"))?;
        let remaining_docs = u64::from(config.documents - document_index);
        let minimum_future = remaining_docs
            .checked_mul(2)
            .ok_or_else(|| budget_error("minimum corpus size arithmetic overflow"))?;
        if remaining < minimum_future {
            return Err(budget_error("byte budget cannot hold remaining documents"));
        }
        let per_payload_budget = remaining
            .saturating_sub(remaining_docs - 1)
            .min(STRICT_PAYLOAD_LIMIT as u64);
        let cap = usize::try_from(per_payload_budget)
            .map_err(|_| budget_error("payload budget exceeds addressable memory"))?;
        let mut writer = JsonWriter::new(cap);
        write_document(
            &mut writer,
            kind,
            config.items_per_document,
            document_index,
            &mut rng,
        )?;
        if writer.bytes.is_empty() {
            return Err(bench_error(
                ErrorCode::InternalError,
                "generator emitted an empty document",
            ));
        }
        if writer.bytes.len() > u32::MAX as usize {
            return Err(bench_error(
                ErrorCode::PayloadTooLarge,
                "payload exceeds ABI length",
            ));
        }
        let length = u64::try_from(writer.bytes.len())
            .map_err(|_| bench_error(ErrorCode::PayloadTooLarge, "payload length exceeds u64"))?;
        used = used
            .checked_add(length)
            .ok_or_else(|| budget_error("corpus byte count overflow"))?;
        if used > budget {
            return Err(budget_error("corpus byte budget exceeded"));
        }
        payloads.push(Payload::from_vec(writer.bytes));
    }
    let (compiled_schema, selection) = match kind {
        WorkloadKind::SmallObject => small_object_schema(config.items_per_document)?,
        WorkloadKind::EscapedStrings => scalar_object_schema(
            "s",
            config.items_per_document,
            ESCAPED_STRINGS_SCHEMA_ID,
            "jmax_bench_escaped_strings",
            |_| ScalarKind::String,
        )?,
        WorkloadKind::Unicode => scalar_object_schema(
            "u",
            config.items_per_document,
            UNICODE_SCHEMA_ID,
            "jmax_bench_unicode",
            |_| ScalarKind::String,
        )?,
        WorkloadKind::AggTrade => agg_trade_schema()?,
        WorkloadKind::NumberArray | WorkloadKind::DepthBook | WorkloadKind::Nested => {
            native_workload_schema(kind, config.items_per_document)?
        }
        _ => return Err(constraint_error("use followup suite for this workload")),
    };
    let schema = Some(compiled_schema.image().clone());
    Ok(Workload {
        name,
        kind,
        payloads,
        schema,
        compiled_schema,
        selection,
    })
}

fn small_object_schema(items: u32) -> Result<(jsonmax::endpoint::CompiledSchema, Selection)> {
    scalar_object_schema(
        "f",
        items,
        SMALL_OBJECT_SCHEMA_ID,
        "jmax_bench_small_object",
        small_field_kind,
    )
}

fn scalar_object_schema(
    prefix: &str,
    items: u32,
    schema_id: u32,
    schema_label: &'static str,
    kind_for_index: fn(u32) -> ScalarKind,
) -> Result<(jsonmax::endpoint::CompiledSchema, Selection)> {
    if items > u32::from(u16::MAX) {
        return Err(constraint_error(
            "schema field count exceeds the ABI required-count limit",
        ));
    }
    let mut fields = Vec::new();
    fields
        .try_reserve_exact(items as usize)
        .map_err(|_| budget_error("schema field allocation failed"))?;
    for index in 0..items {
        let field_id = u16::try_from(index).map_err(|_| {
            bench_error(
                ErrorCode::SchemaImageInvalid,
                "schema field identifier overflow",
            )
        })?;
        fields.push(SchemaFieldIr {
            field_id: FieldId(u32::from(field_id)),
            name: indexed_name(prefix, index)?,
            required: true,
            scalar_kind: Some(kind_for_index(index)),
            format: None,
            min_len: None,
            max_len: None,
            min_i64: None,
            max_i64: None,
        });
    }
    let mut schema_name = String::new();
    schema_name
        .try_reserve_exact(schema_label.len())
        .map_err(|_| budget_error("schema name allocation failed"))?;
    schema_name.push_str(schema_label);
    compile_object_schema(schema_id, schema_name, fields)
}

fn agg_trade_schema() -> Result<(jsonmax::endpoint::CompiledSchema, Selection)> {
    const FIELDS: [(&str, ScalarKind); 11] = [
        ("e", ScalarKind::String),
        ("E", ScalarKind::Number),
        ("s", ScalarKind::String),
        ("a", ScalarKind::Number),
        ("p", ScalarKind::String),
        ("q", ScalarKind::String),
        ("f", ScalarKind::Number),
        ("l", ScalarKind::Number),
        ("T", ScalarKind::Number),
        ("m", ScalarKind::True),
        ("M", ScalarKind::True),
    ];
    let mut fields = Vec::new();
    fields
        .try_reserve_exact(FIELDS.len())
        .map_err(|_| budget_error("schema field allocation failed"))?;
    for (index, (name, scalar_kind)) in FIELDS.iter().enumerate() {
        let mut owned_name = String::new();
        owned_name
            .try_reserve_exact(name.len())
            .map_err(|_| budget_error("schema key allocation failed"))?;
        owned_name.push_str(name);
        fields.push(SchemaFieldIr {
            field_id: FieldId(u32::try_from(index).map_err(|_| {
                bench_error(
                    ErrorCode::SchemaImageInvalid,
                    "trade schema field identifier overflow",
                )
            })?),
            name: owned_name,
            required: true,
            scalar_kind: Some(*scalar_kind),
            format: None,
            min_len: None,
            max_len: None,
            min_i64: None,
            max_i64: None,
        });
    }
    let mut name = String::new();
    name.try_reserve_exact("jmax_bench_agg_trade".len())
        .map_err(|_| budget_error("schema name allocation failed"))?;
    name.push_str("jmax_bench_agg_trade");
    compile_object_schema(jmax_exchange::TRADE_SCHEMA_ID.0, name, fields)
}

fn compile_object_schema(
    schema_id: u32,
    name: String,
    fields: Vec<SchemaFieldIr>,
) -> Result<(jsonmax::endpoint::CompiledSchema, Selection)> {
    let required_count = u16::try_from(fields.len()).map_err(|_| {
        bench_error(
            ErrorCode::SchemaImageInvalid,
            "required field count overflow",
        )
    })?;
    let ir = SchemaIr {
        name,
        schema_id: SchemaId(schema_id),
        required_count,
        reject_unknown: true,
        fields,
    };
    let compiled =
        jsonmax::endpoint::compile_schema(&ir, jmax_abi::schema::CompileLimits::default_limits())?;
    let mut selection_ids = Vec::new();
    selection_ids
        .try_reserve_exact(ir.fields.len())
        .map_err(|_| budget_error("selection allocation failed"))?;
    for field in &ir.fields {
        selection_ids.push(field.field_id);
    }
    Ok((
        compiled,
        Selection {
            field_ids: selection_ids,
            strict_validate_unselected: true,
        },
    ))
}

/// Exact native container shapes for the non-flat standard workloads.
fn native_workload_schema(
    kind: WorkloadKind,
    items: u32,
) -> Result<(jsonmax::endpoint::CompiledSchema, Selection)> {
    use jmax_schema_ir::{
        ArrayShape, ObjectShape, Property, ScalarConstraints, Schema, TypeId, TypeNode, ValueKind,
    };
    let mut nodes = Vec::new();
    nodes
        .try_reserve_exact(68)
        .map_err(|_| budget_error("native schema allocation failed"))?;
    let array = |items, count| {
        TypeNode::Array(ArrayShape { nullable: false,
            items: TypeId(items),
            min_items: Some(count),
            max_items: Some(count),
        })
    };
    let scalar = |kind| TypeNode::Scalar(ScalarConstraints::new(kind));
    let property = |name: &str, field: u32, ty: u32, slot: u32| Property {
        name: String::from(name),
        field_id: FieldId(field),
        ty: TypeId(ty),
        required: true,
        slot: Some(slot),
    };
    let object = |properties| {
        TypeNode::Object(ObjectShape { nullable: false,
            properties,
            reject_unknown: true,
            min_properties: None,
            max_properties: None,
        })
    };
    let schema_id = match kind {
        WorkloadKind::NumberArray => {
            nodes.push(array(1, items));
            nodes.push(scalar(ValueKind::Number));
            0x4a4d_0004
        }
        WorkloadKind::DepthBook => {
            let mut properties = Vec::new();
            properties
                .try_reserve_exact(7)
                .map_err(|_| budget_error("native property allocation failed"))?;
            for (index, (name, ty)) in [
                ("e", 1),
                ("E", 2),
                ("s", 1),
                ("U", 2),
                ("u", 2),
                ("b", 3),
                ("a", 3),
            ]
            .iter()
            .enumerate()
            {
                properties.push(property(name, index as u32, *ty, index as u32));
            }
            nodes.push(object(properties));
            nodes.push(scalar(ValueKind::String));
            nodes.push(scalar(ValueKind::Number));
            nodes.push(array(4, items));
            nodes.push(array(1, 2));
            0x4a4d_0005
        }
        WorkloadKind::Nested => {
            let depth = (items % MAX_NESTING).max(1);
            for level in 0..depth {
                if level % 2 == 0 {
                    nodes.push(array(level + 1, 1));
                } else {
                    let mut properties = Vec::new();
                    properties
                        .try_reserve_exact(1)
                        .map_err(|_| budget_error("native property allocation failed"))?;
                    properties.push(Property {
                        name: String::from("v"),
                        field_id: FieldId(level),
                        ty: TypeId(level + 1),
                        required: true,
                        slot: None,
                    });
                    nodes.push(object(properties));
                }
            }
            nodes.push(scalar(ValueKind::Number));
            0x4a4d_0006
        }
        _ => {
            return Err(constraint_error(
                "workload does not require a native container schema",
            ));
        }
    };
    let graph = Schema {
        name: String::from(kind_name(kind)),
        schema_id: SchemaId(schema_id),
        root: TypeId(0),
        nodes,
        definitions: Vec::new(),
    };
    let compiled = jsonmax::endpoint::compile_native_schema(
        &graph,
        jmax_abi::schema::CompileLimits::default_limits(),
    )?;
    let mut field_ids = Vec::new();
    field_ids
        .try_reserve_exact(compiled.program().program().slots.len())
        .map_err(|_| budget_error("native selection allocation failed"))?;
    field_ids.extend(
        compiled
            .program()
            .program()
            .slots
            .iter()
            .map(|slot| slot.field_id),
    );
    Ok((
        compiled,
        Selection {
            field_ids,
            strict_validate_unselected: true,
        },
    ))
}

fn small_field_kind(index: u32) -> ScalarKind {
    match index % 3 {
        0 => ScalarKind::Number,
        1 => ScalarKind::True,
        _ => ScalarKind::String,
    }
}

fn workload_name(kind: WorkloadKind, config: CorpusConfig) -> Result<String> {
    let mut name = String::new();
    name.try_reserve(96)
        .map_err(|_| budget_error("workload name allocation failed"))?;
    name.push_str(kind_name(kind));
    name.push_str("-seed-");
    push_decimal(&mut name, config.seed)?;
    name.push_str("-docs-");
    push_decimal(&mut name, u64::from(config.documents))?;
    name.push_str("-items-");
    push_decimal(&mut name, u64::from(config.items_per_document))?;
    Ok(name)
}

fn kind_name(kind: WorkloadKind) -> &'static str {
    match kind {
        WorkloadKind::SmallObject => "small_object",
        WorkloadKind::EscapedStrings => "escaped_strings",
        WorkloadKind::Unicode => "unicode",
        WorkloadKind::NumberArray => "number_array",
        WorkloadKind::DepthBook => "depth_book",
        WorkloadKind::AggTrade => "agg_trade",
        WorkloadKind::Nested => "nested",
        _ => "followup",
    }
}

fn write_document(
    writer: &mut JsonWriter,
    kind: WorkloadKind,
    items: u32,
    document_index: u32,
    rng: &mut DeterministicRng,
) -> Result<()> {
    match kind {
        WorkloadKind::SmallObject => {
            writer.byte(b'{')?;
            for index in 0..items {
                if index != 0 {
                    writer.byte(b',')?;
                }
                write_quoted(writer, &indexed_name("f", index)?)?;
                writer.byte(b':')?;
                match small_field_kind(index) {
                    ScalarKind::Number => {
                        let variation = (rng.next_u64() % 1_000) as i64;
                        writer.decimal(
                            i64::from(document_index) * 1_000_003 + i64::from(index) + variation,
                        )?;
                    }
                    ScalarKind::True => writer.extend(b"true")?,
                    ScalarKind::String => {
                        writer.byte(b'"')?;
                        writer.extend(b"v")?;
                        writer.decimal(i64::from(document_index))?;
                        writer.byte(b'-')?;
                        writer.decimal(i64::from(index))?;
                        writer.byte(b'-')?;
                        writer.unsigned(rng.next_u64() % 1_000_000)?;
                        writer.byte(b'"')?;
                    }
                    _ => {
                        return Err(bench_error(
                            ErrorCode::InternalError,
                            "invalid small object scalar kind",
                        ));
                    }
                }
            }
            writer.byte(b'}')
        }
        WorkloadKind::EscapedStrings => {
            writer.byte(b'{')?;
            for index in 0..items {
                if index != 0 {
                    writer.byte(b',')?;
                }
                write_quoted(writer, &indexed_name("s", index)?)?;
                writer.extend(b":\"")?;
                writer.extend(b"quote=\\\" slash=\\\\ controls=")?;
                writer.extend(b"\\u0001\\u001f pair=\\ud83d\\ude00 index=")?;
                writer.decimal(i64::from(document_index))?;
                writer.byte(b'-')?;
                writer.decimal(i64::from(index))?;
                writer.byte(b'-')?;
                writer.unsigned(rng.next_u64() % 1_000_000)?;
                writer.byte(b'"')?;
            }
            writer.byte(b'}')
        }
        WorkloadKind::Unicode => {
            writer.byte(b'{')?;
            for index in 0..items {
                if index != 0 {
                    writer.byte(b',')?;
                }
                write_quoted(writer, &indexed_name("u", index)?)?;
                writer.extend(
                    b":\"BMP-\xc3\xa9-\xe6\xbc\xa2-\xce\xa9 nonBMP-\xf0\x9f\xa7\xad index=",
                )?;
                writer.decimal(i64::from(document_index))?;
                writer.byte(b'-')?;
                writer.decimal(i64::from(index))?;
                writer.byte(b'-')?;
                writer.unsigned(rng.next_u64() % 1_000_000)?;
                writer.byte(b'"')?;
            }
            writer.byte(b'}')
        }
        WorkloadKind::NumberArray => {
            writer.byte(b'[')?;
            for index in 0..items {
                if index != 0 {
                    writer.byte(b',')?;
                }
                write_number_case(writer, index, document_index, rng)?;
            }
            writer.byte(b']')
        }
        WorkloadKind::DepthBook => write_depth_book(writer, items, document_index, rng),
        WorkloadKind::AggTrade => write_agg_trade(writer, document_index, rng),
        WorkloadKind::Nested => write_nested(writer, items, document_index, rng),
        _ => Err(constraint_error("use followup suite for this workload")),
    }
}

fn write_depth_book(
    writer: &mut JsonWriter,
    items: u32,
    doc: u32,
    rng: &mut DeterministicRng,
) -> Result<()> {
    let update = u64::from(doc)
        .checked_mul(100_000)
        .and_then(|n| n.checked_add(rng.next_u64() % 10_000))
        .ok_or_else(|| budget_error("depth update identifier overflow"))?;
    writer.extend(b"{\"e\":\"depthUpdate\",\"E\":")?;
    writer.unsigned(update)?;
    writer.extend(b",\"s\":\"BTCUSDT\",\"U\":")?;
    writer.unsigned(update)?;
    writer.extend(b",\"u\":")?;
    writer.unsigned(
        update
            .checked_add(u64::from(items))
            .ok_or_else(|| budget_error("depth update identifier overflow"))?,
    )?;
    writer.extend(b",\"b\":[")?;
    write_book_levels(writer, items, rng)?;
    writer.extend(b"],\"a\":[")?;
    write_book_levels(writer, items, rng)?;
    writer.extend(b"]}")
}

fn write_book_levels(
    writer: &mut JsonWriter,
    items: u32,
    rng: &mut DeterministicRng,
) -> Result<()> {
    for index in 0..items {
        if index != 0 {
            writer.byte(b',')?;
        }
        let price = 100_000_000u64
            .checked_add(rng.next_u64() % 100_000_000)
            .ok_or_else(|| budget_error("price generation overflow"))?;
        let quantity = 1u64
            .checked_add(rng.next_u64() % 100_000)
            .ok_or_else(|| budget_error("quantity generation overflow"))?;
        writer.extend(b"[\"")?;
        writer.fixed_decimal(price, 8)?;
        writer.extend(b"\",\"")?;
        writer.fixed_decimal(quantity, 4)?;
        writer.extend(b"\"]")?;
    }
    Ok(())
}

fn write_agg_trade(writer: &mut JsonWriter, doc: u32, rng: &mut DeterministicRng) -> Result<()> {
    let trade_id = u64::from(doc)
        .checked_mul(10_000)
        .and_then(|n| n.checked_add(rng.next_u64() % 10_000))
        .ok_or_else(|| budget_error("trade identifier overflow"))?;
    writer.extend(b"{\"e\":\"aggTrade\",\"E\":")?;
    writer.unsigned(1_700_000_000_000 + u64::from(doc))?;
    writer.extend(b",\"s\":\"BTCUSDT\",\"a\":")?;
    writer.unsigned(trade_id)?;
    writer.extend(b",\"p\":\"")?;
    writer.fixed_decimal(3_000_000_000_000 + rng.next_u64() % 100_000_000, 8)?;
    writer.extend(b"\",\"q\":\"")?;
    writer.fixed_decimal(1 + rng.next_u64() % 10_000_000, 8)?;
    writer.extend(b"\",\"f\":")?;
    writer.unsigned(trade_id)?;
    writer.extend(b",\"l\":")?;
    writer.unsigned(trade_id)?;
    writer.extend(b",\"T\":")?;
    writer.unsigned(1_700_000_000_000 + u64::from(doc))?;
    writer.extend(b",\"m\":true")?;
    writer.extend(b",\"M\":true}")
}

fn write_number_case(
    writer: &mut JsonWriter,
    index: u32,
    doc: u32,
    rng: &mut DeterministicRng,
) -> Result<()> {
    const CASES: [&[u8]; 8] = [
        b"-9223372036854775808",
        b"9223372036854775807",
        b"0",
        b"18446744073709551615",
        b"-0.125",
        b"6.022e23",
        b"1.25E-8",
        b"9007199254740993",
    ];
    let case_index = (index as usize + doc as usize) % CASES.len();
    if index < CASES.len() as u32 {
        if case_index == 2 {
            if rng.next_u64() & 1 == 0 {
                writer.extend(b"0")
            } else {
                writer.extend(b"0.0")
            }
        } else {
            let _ = rng.next_u64();
            writer.extend(CASES[case_index])
        }
    } else {
        let value = rng.next_u64() % 1_000_000_000;
        writer.unsigned(value)
    }
}

fn write_nested(
    writer: &mut JsonWriter,
    items: u32,
    doc: u32,
    rng: &mut DeterministicRng,
) -> Result<()> {
    let depth = (items % MAX_NESTING).max(1);
    for level in 0..depth {
        if level % 2 == 0 {
            writer.byte(b'[')?;
        } else {
            writer.extend(b"{\"v\":")?;
        }
    }
    writer.unsigned(u64::from(doc).wrapping_add(rng.next_u64()))?;
    for level in (0..depth).rev() {
        if level % 2 == 0 {
            writer.byte(b']')?;
        } else {
            writer.byte(b'}')?;
        }
    }
    Ok(())
}

fn indexed_name(prefix: &str, index: u32) -> Result<String> {
    let mut name = String::new();
    name.try_reserve(prefix.len() + 10)
        .map_err(|_| budget_error("field name allocation failed"))?;
    name.push_str(prefix);
    push_padded_decimal(&mut name, u64::from(index), 6)?;
    Ok(name)
}

fn push_decimal(target: &mut String, value: u64) -> Result<()> {
    let mut digits = [0u8; 20];
    let mut cursor = digits.len();
    let mut n = value;
    loop {
        cursor -= 1;
        digits[cursor] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    target
        .try_reserve(digits.len() - cursor)
        .map_err(|_| budget_error("integer formatting allocation failed"))?;
    for digit in &digits[cursor..] {
        target.push(char::from(*digit));
    }
    Ok(())
}

fn push_padded_decimal(target: &mut String, value: u64, width: usize) -> Result<()> {
    let mut digits = [0u8; 20];
    let mut cursor = digits.len();
    let mut n = value;
    loop {
        cursor -= 1;
        digits[cursor] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    let count = digits.len() - cursor;
    let padding = width.saturating_sub(count);
    target
        .try_reserve(padding + count)
        .map_err(|_| budget_error("integer formatting allocation failed"))?;
    for _ in 0..padding {
        target.push('0');
    }
    for digit in &digits[cursor..] {
        target.push(char::from(*digit));
    }
    Ok(())
}

fn write_quoted(writer: &mut JsonWriter, text: &str) -> Result<()> {
    writer.byte(b'"')?;
    for byte in text.bytes() {
        match byte {
            b'"' => writer.extend(b"\\\"")?,
            b'\\' => writer.extend(b"\\\\")?,
            0x00..=0x1f => {
                writer.extend(b"\\u00")?;
                writer.byte(HEX[(byte >> 4) as usize])?;
                writer.byte(HEX[(byte & 0x0f) as usize])?;
            }
            _ => writer.byte(byte)?,
        }
    }
    writer.byte(b'"')
}

const HEX: &[u8; 16] = b"0123456789abcdef";

struct JsonWriter {
    bytes: Vec<u8>,
    limit: usize,
}

impl JsonWriter {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
        }
    }

    fn reserve_for(&mut self, additional: usize) -> Result<()> {
        let end = self
            .bytes
            .len()
            .checked_add(additional)
            .ok_or_else(|| budget_error("payload size arithmetic overflow"))?;
        if end > self.limit {
            return Err(budget_error(
                "corpus byte budget exceeded while generating payload",
            ));
        }
        self.bytes
            .try_reserve(additional)
            .map_err(|_| budget_error("payload allocation failed"))
    }

    fn byte(&mut self, byte: u8) -> Result<()> {
        self.reserve_for(1)?;
        self.bytes.push(byte);
        Ok(())
    }

    fn extend(&mut self, bytes: &[u8]) -> Result<()> {
        self.reserve_for(bytes.len())?;
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }

    fn decimal(&mut self, value: i64) -> Result<()> {
        if value < 0 {
            self.byte(b'-')?;
            self.unsigned(value.unsigned_abs())
        } else {
            self.unsigned(value as u64)
        }
    }

    fn unsigned(&mut self, value: u64) -> Result<()> {
        let mut digits = [0u8; 20];
        let mut cursor = digits.len();
        let mut n = value;
        loop {
            cursor -= 1;
            digits[cursor] = b'0' + (n % 10) as u8;
            n /= 10;
            if n == 0 {
                break;
            }
        }
        self.extend(&digits[cursor..])
    }

    fn fixed_decimal(&mut self, value: u64, scale: u32) -> Result<()> {
        let divisor = 10u64
            .checked_pow(scale)
            .ok_or_else(|| budget_error("decimal scale overflow"))?;
        self.unsigned(value / divisor)?;
        self.byte(b'.')?;
        let mut fraction = value % divisor;
        let mut digits = [b'0'; 20];
        for slot in digits.iter_mut().rev().take(scale as usize) {
            *slot = b'0' + (fraction % 10) as u8;
            fraction /= 10;
        }
        let start = digits.len() - scale as usize;
        self.extend(&digits[start..])
    }
}

fn constraint_error(detail: &'static str) -> crate::Error {
    bench_error(ErrorCode::ConstraintViolation, detail)
}

fn budget_error(detail: &'static str) -> crate::Error {
    bench_error(ErrorCode::BudgetExhausted, detail)
}

pub mod followup {
    //! The seven follow-up workloads, with exact native shape schemas.
    use crate::{
        CorpusConfig, ErrorCode, Payload, Result, Selection, Workload, WorkloadKind, bench_error,
    };
    use alloc::{string::String, vec, vec::Vec};
    use core::fmt::Write;
    use jmax_abi::schema::{FieldId, SchemaId};
    use jmax_schema_ir::{
        ArrayShape, ObjectShape, Property, ScalarConstraints, Schema, TypeId, TypeNode, ValueKind,
    };

    pub const KINDS: [WorkloadKind; 7] = [
        WorkloadKind::NagoyaAuth,
        WorkloadKind::ExchangeOrder,
        WorkloadKind::RpcEnvelope,
        WorkloadKind::LargeCatalog,
        WorkloadKind::VectorCoordinates,
        WorkloadKind::DeepNesting,
        WorkloadKind::EscapedUnicode,
    ];
    pub fn is_followup(kind: WorkloadKind) -> bool {
        KINDS.contains(&kind)
    }
    pub fn name(kind: WorkloadKind) -> &'static str {
        match kind {
            WorkloadKind::NagoyaAuth => "NagoyaAuth",
            WorkloadKind::ExchangeOrder => "ExchangeOrder",
            WorkloadKind::RpcEnvelope => "RpcEnvelope",
            WorkloadKind::LargeCatalog => "LargeCatalog",
            WorkloadKind::VectorCoordinates => "VectorCoordinates",
            WorkloadKind::DeepNesting => "DeepNesting",
            WorkloadKind::EscapedUnicode => "EscapedUnicode",
            _ => "Legacy",
        }
    }
    pub fn make_suite(config: CorpusConfig) -> Result<Vec<Workload>> {
        let mut remaining = config.max_bytes;
        let mut suite = Vec::new();
        for kind in KINDS {
            let workload = make_workload(
                kind,
                CorpusConfig {
                    max_bytes: remaining,
                    ..config
                },
            )?;
            remaining = remaining
                .checked_sub(workload.total_bytes()?)
                .ok_or_else(budget)?;
            suite.push(workload);
        }
        Ok(suite)
    }
    pub fn make_workload(kind: WorkloadKind, config: CorpusConfig) -> Result<Workload> {
        if !is_followup(kind)
            || config.documents == 0
            || config.items_per_document == 0
            || config.documents > 1_000_000
            || config.items_per_document > 1_000_000
        {
            return Err(bench_error(
                ErrorCode::ConstraintViolation,
                "invalid followup corpus dimensions",
            ));
        }
        let mut nodes = graph(kind, config.items_per_document);
        let mut field_id = 0u32;
        for node in &mut nodes {
            if let TypeNode::Object(object) = node {
                for property in &mut object.properties {
                    property.field_id = FieldId(field_id);
                    field_id += 1;
                }
            }
        }
        let schema = Schema {
            name: String::from(name(kind)),
            schema_id: SchemaId(0x4a4e_0000 + kind as u32),
            root: TypeId(0),
            nodes,
            definitions: Vec::new(),
        };
        let compiled_schema = jsonmax::endpoint::compile_native_schema(
            &schema,
            jsonmax::CompileLimits::default_limits(),
        )?;
        let selection = Selection {
            field_ids: compiled_schema
                .program()
                .program()
                .slots
                .iter()
                .map(|s| s.field_id)
                .collect(),
            strict_validate_unselected: true,
        };
        let mut payloads = Vec::new();
        let mut total = 0u64;
        for doc in 0..config.documents {
            let limit = config
                .max_bytes
                .checked_sub(total)
                .ok_or_else(budget)?
                .min(16 * 1024 * 1024);
            let bytes = document(kind, config.items_per_document, doc, config.seed, limit)?;
            total = total.checked_add(bytes.len() as u64).ok_or_else(budget)?;
            if total > config.max_bytes {
                return Err(budget());
            }
            payloads.push(Payload::from_vec(bytes.into_bytes()));
        }
        Ok(Workload {
            name: String::from(name(kind)),
            kind,
            payloads,
            schema: None,
            compiled_schema,
            selection,
        })
    }
    fn budget() -> crate::Error {
        bench_error(
            ErrorCode::BudgetExhausted,
            "followup corpus budget exceeded",
        )
    }
    fn scalar(kind: ValueKind) -> TypeNode {
        TypeNode::Scalar(ScalarConstraints::new(kind))
    }
    fn array(ty: u32, count: u32) -> TypeNode {
        TypeNode::Array(ArrayShape { nullable: false,
            items: TypeId(ty),
            min_items: Some(count),
            max_items: Some(count),
        })
    }
    fn object(fields: &[(&str, u32)], capture: bool) -> TypeNode {
        TypeNode::Object(ObjectShape { nullable: false,
            properties: fields
                .iter()
                .enumerate()
                .map(|(i, (name, ty))| Property {
                    name: String::from(*name),
                    field_id: FieldId(i as u32),
                    ty: TypeId(*ty),
                    required: true,
                    slot: capture.then_some(i as u32),
                })
                .collect(),
            reject_unknown: true,
            min_properties: None,
            max_properties: None,
        })
    }
    fn graph(kind: WorkloadKind, items: u32) -> Vec<TypeNode> {
        let string = || scalar(ValueKind::String);
        let number = || scalar(ValueKind::Number);
        let boolean = || scalar(ValueKind::Boolean);
        match kind {
            WorkloadKind::NagoyaAuth => vec![
                object(
                    &[("user", 1), ("token", 1), ("expires", 2), ("admin", 3)],
                    true,
                ),
                string(),
                number(),
                boolean(),
            ],
            WorkloadKind::ExchangeOrder => vec![
                object(
                    &[
                        ("symbol", 1),
                        ("side", 1),
                        ("price", 1),
                        ("quantity", 1),
                        ("id", 2),
                    ],
                    true,
                ),
                string(),
                number(),
            ],
            WorkloadKind::RpcEnvelope => vec![
                object(
                    &[("jsonrpc", 1), ("id", 2), ("method", 1), ("params", 3)],
                    true,
                ),
                string(),
                number(),
                object(&[("account", 1), ("limit", 2), ("active", 4)], false),
                boolean(),
            ],
            WorkloadKind::LargeCatalog => vec![
                object(&[("catalog", 1), ("version", 4)], true),
                array(2, items * 16),
                object(
                    &[("id", 4), ("name", 3), ("price", 4), ("active", 5)],
                    false,
                ),
                string(),
                number(),
                boolean(),
            ],
            WorkloadKind::VectorCoordinates => vec![
                object(&[("coordinates", 1)], true),
                array(2, items * 3),
                number(),
            ],
            WorkloadKind::EscapedUnicode => {
                vec![object(&[("text", 1), ("id", 2)], true), string(), number()]
            }
            WorkloadKind::DeepNesting => {
                let depth = items.clamp(1, 64);
                let mut nodes = vec![object(&[("root", 1)], true)];
                nodes.extend((0..depth).map(|level| {
                    if level % 2 == 0 {
                        array(level + 2, 1)
                    } else {
                        object(&[("v", level + 2)], false)
                    }
                }));
                nodes.push(number());
                nodes
            }
            _ => Vec::new(),
        }
    }
    fn document(kind: WorkloadKind, items: u32, doc: u32, seed: u64, limit: u64) -> Result<String> {
        let id = (seed % 1_000_000) + u64::from(doc);
        let minimum = match kind {
            WorkloadKind::LargeCatalog => u64::from(items) * 16 * 40,
            WorkloadKind::VectorCoordinates => u64::from(items) * 3 * 2,
            WorkloadKind::EscapedUnicode => u64::from(items) * 50,
            WorkloadKind::DeepNesting => u64::from(items.clamp(1, 64)) * 2,
            _ => 1,
        };
        if minimum > limit {
            return Err(budget());
        }
        let mut out = String::new();
        match kind {
            WorkloadKind::NagoyaAuth => write!(out,"{{\"user\":\"user-{doc}\",\"token\":\"tok-{id:016x}\",\"expires\":{},\"admin\":false}}",1_800_000_000+id),
            WorkloadKind::ExchangeOrder => write!(out,"{{\"symbol\":\"BTCUSDT\",\"side\":\"BUY\",\"price\":\"30000.125\",\"quantity\":\"0.0125\",\"id\":{id}}}"),
            WorkloadKind::RpcEnvelope => write!(out,"{{\"jsonrpc\":\"2.0\",\"id\":{id},\"method\":\"submit\",\"params\":{{\"account\":\"acct-{doc}\",\"limit\":100,\"active\":true}}}}"),
            WorkloadKind::LargeCatalog => {
                out.push_str("{\"catalog\":[");
                for i in 0..items*16 {
                    if i>0 { out.push(','); }
                    write!(out,"{{\"id\":{},\"name\":\"product-{doc}-{i}\",\"price\":{}.125,\"active\":true}}",id+u64::from(i),i+1).map_err(|_| budget())?;
                    if out.len() as u64 > limit { return Err(budget()); }
                }
                write!(out,"],\"version\":{id}}}")
            }
            WorkloadKind::VectorCoordinates => {
                out.push_str("{\"coordinates\":[");
                for i in 0..items*3 {
                    if i>0 { out.push(','); }
                    write!(out,"-{}.125",id%1000+u64::from(i)).map_err(|_|budget())?;
                    if out.len() as u64 > limit { return Err(budget()); }
                }
                out.push_str("]}"); Ok(())
            }
            WorkloadKind::DeepNesting => {
                let depth=items.clamp(1,64);
                out.push_str("{\"root\":");
                for i in 0..depth { out.push_str(if i%2==0 { "[" } else { "{\"v\":" }); }
                write!(out,"{id}").map_err(|_|budget())?;
                for i in (0..depth).rev() { out.push(if i%2==0 { ']' } else { '}' }); }
                out.push('}');
                Ok(())
            }
            WorkloadKind::EscapedUnicode => {
                out.push_str("{\"text\":\"");
                for _ in 0..items { out.push_str("quote=\\\" slash=\\\\ newline=\\n 漢字-é-🧭 pair=\\ud83d\\ude00 "); if out.len() as u64 > limit { return Err(budget()); } }
                write!(out,"doc-{doc}\",\"id\":{id}}}")
            }
            _ => return Err(bench_error(ErrorCode::ConstraintViolation,"unsupported followup kind")),
        }.map_err(|_|budget())?;
        if out.len() as u64 > limit {
            return Err(budget());
        }
        Ok(out)
    }
}
