//! Deterministic lowering from resolved schema graphs to bounded state programs.
//!
//! State ranges are independent event handlers. In particular, `EmitSlot` is a
//! property-completion action, not an instruction to execute on object entry.

use super::*;
use jmax_schema_ir::{ObjectShape, Property, TypeNode};

/// Validate policy, canonicalize, and lower the reachable graph without issuing
/// an image-validation witness. All state numbers follow canonical BFS order.
pub fn lower(schema: &ResolvedSchema, options: &CompilerOptions) -> Result<CompiledProgram> {
    lower_inner(schema, options).map_err(|mut error| {
        error.source = ErrorSource::SchemaCompile;
        error.byte_offset = 0;
        error.schema_id = schema.schema().schema_id.0;
        error
    })
}

/// Lower and then obtain the witness exclusively through image validation.
pub fn compile(schema: &ResolvedSchema, options: &CompilerOptions) -> Result<ValidatedProgram> {
    image::validate(lower(schema, options)?, options)
}

fn budget() -> Error {
    compile_error(ErrorCode::BudgetExhausted, "schema compilation resource limit exceeded")
}

fn invalid(detail: &'static str) -> Error {
    compile_error(ErrorCode::ConstraintViolation, detail)
}

fn unsupported(field: FieldId, detail: &'static str) -> Error {
    compile_error(ErrorCode::UnsupportedSchemaFeature, detail).with_schema(0, field.0)
}

fn count(value: usize) -> Result<u32> {
    u32::try_from(value).map_err(|_| budget())
}

fn index(value: u32) -> Result<usize> {
    usize::try_from(value).map_err(|_| budget())
}

fn add(left: u32, right: u32) -> Result<u32> {
    left.checked_add(right).ok_or_else(budget)
}

fn limited(value: u32, limit: u32) -> Result<()> {
    if value > limit { Err(budget()) } else { Ok(()) }
}

/// Every owned allocation in this pass is reserved fallibly before mutation.
fn reserve<T>(values: &mut Vec<T>, additional: usize) -> Result<()> {
    count(values.len().checked_add(additional).ok_or_else(budget)?)?;
    values.try_reserve(additional).map_err(|_| budget())
}

fn push<T>(values: &mut Vec<T>, value: T) -> Result<()> {
    reserve(values, 1)?;
    values.push(value);
    Ok(())
}

fn filled<T: Copy>(len: usize, value: T) -> Result<Vec<T>> {
    let mut values = Vec::new();
    reserve(&mut values, len)?;
    values.resize(len, value);
    Ok(values)
}

fn copy_name(name: &str) -> Result<String> {
    count(name.len())?;
    let mut owned = String::new();
    owned.try_reserve_exact(name.len()).map_err(|_| budget())?;
    owned.push_str(name);
    Ok(owned)
}

struct NodePlan<'a> {
    node: &'a TypeNode,
    // Sorted property references and their corresponding children. An array
    // has one child and no properties; scalars and Any have no children.
    properties: Vec<&'a Property>,
    children: Vec<StateId>,
}

fn sorted_properties(shape: &ObjectShape) -> Result<Vec<&Property>> {
    let mut properties = Vec::new();
    reserve(&mut properties, shape.properties.len())?;
    properties.extend(shape.properties.iter());
    properties.sort_unstable_by(|left, right| left.name.as_bytes().cmp(right.name.as_bytes()));
    Ok(properties)
}

fn discover(
    schema: &ResolvedSchema,
    ty: TypeId,
    numbers: &mut [Option<StateId>],
    order: &mut Vec<TypeId>,
    options: &CompilerOptions,
) -> Result<StateId> {
    let target = schema.target(ty).ok_or_else(|| invalid("invalid resolved type target"))?;
    let entry = numbers.get_mut(index(target.0)?)
        .ok_or_else(|| invalid("resolved type target is outside the graph"))?;
    if let Some(id) = *entry {
        return Ok(id);
    }
    let id = StateId(count(order.len())?);
    limited(add(id.0, 1)?, options.limits.max_states)?;
    push(order, target)?;
    *entry = Some(id);
    Ok(id)
}

fn plan_graph<'a>(schema: &'a ResolvedSchema, options: &CompilerOptions) -> Result<Vec<NodePlan<'a>>> {
    let mut numbers = filled(schema.schema().nodes.len(), None)?;
    let mut order = Vec::new();
    discover(schema, schema.schema().root, &mut numbers, &mut order, options)?;
    let mut plans = Vec::new();
    let mut cursor = 0usize;
    let mut total_keys = 0u32;
    while let Some(&ty) = order.get(cursor) {
        let node = schema.schema().nodes.get(index(ty.0)?)
            .ok_or_else(|| invalid("invalid reachable type"))?;
        let mut properties = Vec::new();
        let mut children = Vec::new();
        match node {
            TypeNode::Object(shape) => {
                total_keys = add(total_keys, count(shape.properties.len())?)?;
                limited(total_keys, options.limits.max_keys)?;
                properties = sorted_properties(shape)?;
                reserve(&mut children, properties.len())?;
                let mut required = 0u32;
                for property in &properties {
                    u16::try_from(property.field_id.0).map_err(|_| {
                        unsupported(property.field_id, "field identifier does not fit the image ABI")
                    })?;
                    if property.required {
                        required = add(required, 1)?;
                        if required > u32::from(NO_REQUIRED_BIT) {
                            return Err(unsupported(property.field_id, "too many required object properties"));
                        }
                    }
                    children.push(discover(schema, property.ty, &mut numbers, &mut order, options)?);
                }
            }
            TypeNode::Array(shape) => {
                let child = discover(schema, shape.items, &mut numbers, &mut order, options)?;
                push(&mut children, child)?;
            }
            TypeNode::Any | TypeNode::Scalar(_) | TypeNode::Enum(_) => {}
            TypeNode::Ref(_) => return Err(invalid("resolved graph contains an uncollapsed alias")),
        }
        push(&mut plans, NodePlan { node, properties, children })?;
        cursor = cursor.checked_add(1).ok_or_else(budget)?;
    }
    Ok(plans)
}

/// Propagate only newly discovered path contributions. Every node increases
/// from zero to one to two at most, so cycles terminate and each edge is visited
/// at most twice. Array edges contribute two immediately. Parallel property
/// edges remain distinct paths even when they have the same target type.
fn path_counts(plans: &[NodePlan<'_>]) -> Result<Vec<u8>> {
    let mut paths = filled(plans.len(), 0u8)?;
    *paths.first_mut().ok_or_else(|| invalid("schema has no root state"))? = 1;
    let mut pending = Vec::new();
    push(&mut pending, (0usize, 1u8))?;
    let mut cursor = 0usize;
    while let Some(&(owner, delta)) = pending.get(cursor) {
        let plan = plans.get(owner).ok_or_else(|| invalid("invalid path owner"))?;
        let contribution = if matches!(plan.node, TypeNode::Array(_)) { 2 } else { delta };
        for child in &plan.children {
            let child_index = index(child.0)?;
            let seen = paths.get_mut(child_index).ok_or_else(|| invalid("invalid path target"))?;
            let next = seen.saturating_add(contribution).min(2);
            if next > *seen {
                let change = next.checked_sub(*seen).ok_or_else(budget)?;
                *seen = next;
                push(&mut pending, (child_index, change))?;
            }
        }
        cursor = cursor.checked_add(1).ok_or_else(budget)?;
    }
    Ok(paths)
}

/// A property of an object that is the sole item type of an array reached by
/// exactly one path may carry a slot even though the object occurs once per
/// element: the rows API stages those slots on every element close. Anything
/// else that can occur more than once is rejected.
fn row_item_owner(plans: &[NodePlan<'_>], paths: &[u8], owner: usize) -> bool {
    let mut parent = None;
    let mut incoming = 0usize;
    for (candidate, plan) in plans.iter().enumerate() {
        for child in &plan.children {
            if child.0 as usize == owner {
                incoming += 1;
                parent = Some(candidate);
            }
        }
    }
    let Some(parent) = parent else { return false };
    if incoming != 1 || !matches!(plans[parent].node, TypeNode::Array(_)) || paths[parent] != 1 {
        return false;
    }
    plans
        .iter()
        .flat_map(|plan| plan.children.iter())
        .filter(|child| child.0 as usize == parent)
        .count()
        == 1
}

fn plan_slots(plans: &[NodePlan<'_>]) -> Result<Vec<SlotSpec>> {
    let paths = path_counts(plans)?;
    let mut slots = Vec::new();
    for (owner, plan) in plans.iter().enumerate() {
        for (property, &state) in plan.properties.iter().zip(&plan.children) {
            if let Some(slot) = property.slot {
                if paths.get(owner).copied() != Some(1) && !row_item_owner(plans, &paths, owner) {
                    return Err(unsupported(property.field_id, "selected property can occur more than once"));
                }
                push(&mut slots, SlotSpec { slot, field_id: property.field_id, state })?;
            }
        }
    }
    slots.sort_unstable_by_key(|spec| spec.slot);
    for (expected, spec) in slots.iter().enumerate() {
        if spec.slot != count(expected)? {
            return Err(invalid("selected slots must densely cover zero through slot count")
                .with_schema(0, spec.field_id.0));
        }
    }
    Ok(slots)
}

/// Deduplicate by text, retaining the earliest canonical state, then compile
/// in that encounter order. The returned lookup is sorted by source text.
fn compile_patterns<'a>(
    plans: &[NodePlan<'a>],
    options: &CompilerOptions,
) -> Result<(Vec<PatternProgram>, Vec<(&'a str, u32)>)> {
    let mut sources = Vec::new();
    for (state, plan) in plans.iter().enumerate() {
        if let TypeNode::Scalar(constraints) = plan.node {
            if let Some(source) = constraints.pattern.as_deref() {
                push(&mut sources, (source, count(state)?))?;
            }
        }
    }
    sources.sort_unstable_by(|left, right| left.0.cmp(right.0).then(left.1.cmp(&right.1)));
    sources.dedup_by(|later, earlier| later.0 == earlier.0);
    sources.sort_unstable_by_key(|source| source.1);
    let mut programs = Vec::new();
    reserve(&mut programs, sources.len())?;
    let mut total_ops = 0u32;
    for (source, pattern_index) in &mut sources {
        let program = patterns::compile(source, options.limits.max_pattern_states, options.max_pattern_steps)?;
        let ops = count(program.ops.len())?;
        limited(ops, options.limits.max_pattern_states)?;
        total_ops = add(total_ops, ops)?;
        limited(total_ops, options.limits.max_pattern_states)?;
        *pattern_index = count(programs.len())?;
        programs.push(program);
    }
    sources.sort_unstable_by(|left, right| left.0.cmp(right.0));
    Ok((programs, sources))
}

fn scalar_constraints(source: &ScalarConstraints, patterns: &[(&str, u32)]) -> Result<ConstraintSet> {
    let pattern = match source.pattern.as_deref() {
        Some(text) => {
            let found = patterns.binary_search_by(|entry| entry.0.cmp(text))
                .map_err(|_| invalid("pattern was not compiled"))?;
            Some(patterns.get(found).ok_or_else(|| invalid("invalid pattern index"))?.1)
        }
        None => None,
    };
    let format = match source.format.as_deref() {
        // Policy validation already rejected unknown formats unless annotation
        // was requested. Even long unknown annotations have no registry lookup.
        Some(name) if jmax_schema_ir::policy::known_format(name) => {
            Some(jmax_format::registry::lookup(name)?)
        }
        _ => None,
    };
    Ok(ConstraintSet {
        min_i64: source.min_i64,
        max_i64: source.max_i64,
        decimal: source.decimal.clone(),
        min_count: source.min_length,
        max_count: source.max_length,
        pattern,
        format,
        nullable: source.nullable,
        enum_variants: Vec::new(),
    })
}

fn scalar_tag(kind: ValueKind) -> u64 {
    match kind {
        ValueKind::Null => 0,
        ValueKind::Boolean => 1,
        ValueKind::Integer => 2,
        ValueKind::Number => 3,
        ValueKind::Decimal => 4,
        ValueKind::String => 5,
    }
}

fn emit(
    insns: &mut Vec<InsnWord>,
    options: &CompilerOptions,
    opcode: Opcode,
    field_id: u16,
    arg0: u32,
    arg1: u64,
) -> Result<()> {
    limited(add(count(insns.len())?, 1)?, options.limits.max_insns)?;
    push(insns, InsnWord { opcode: opcode_byte(opcode), flags: 0, field_id, arg0, arg1 })
}

fn append_keys(
    program: &mut CompiledProgram,
    plan: &NodePlan<'_>,
    options: &CompilerOptions,
) -> Result<(u32, u16, Opcode)> {
    let mut specs = Vec::new();
    reserve(&mut specs, plan.properties.len())?;
    for (property, &target) in plan.properties.iter().zip(&plan.children) {
        specs.push(KeySpec {
            name: copy_name(&property.name)?,
            field_id: property.field_id,
            required: property.required,
            target,
        });
    }
    let mut built = keys::build(&specs, options)?;
    let key_base = count(program.image.keys.len())?;
    let byte_base = count(program.image.key_bytes.len())?;
    let key_count = count(built.entries.len())?;
    limited(add(key_base, key_count)?, options.limits.max_keys)?;
    let byte_end = add(byte_base, count(built.bytes.len())?)?;
    for entry in &mut built.entries {
        entry.span.offset = add(byte_base, entry.span.offset)?;
        let end = entry.span.end().ok_or_else(budget)?;
        if end > byte_end {
            return Err(invalid("key builder produced an invalid byte span"));
        }
    }
    let match_opcode = match &mut built.strategy {
        KeyStrategy::Swar => Opcode::MatchKeySwar,
        KeyStrategy::PerfectHash { slots, .. }
        | KeyStrategy::BucketedPerfectHash { slots, .. } => {
            for slot in slots {
                if *slot != EMPTY_KEY_SLOT {
                    if *slot >= key_count {
                        return Err(invalid("key builder produced an invalid local index"));
                    }
                    *slot = add(key_base, *slot)?;
                }
            }
            Opcode::MatchKeyPhf
        }
    };
    let table = count(program.key_tables.len())?;
    reserve(&mut program.image.keys, built.entries.len())?;
    reserve(&mut program.image.key_bytes, built.bytes.len())?;
    program.image.keys.append(&mut built.entries);
    program.image.key_bytes.append(&mut built.bytes);
    push(&mut program.key_tables, KeyTable { first_key: key_base, key_count, strategy: built.strategy })?;
    Ok((table, built.required_count, match_opcode))
}

fn lower_inner(schema: &ResolvedSchema, options: &CompilerOptions) -> Result<CompiledProgram> {
    // Both passes finish before instructions or executable side tables exist.
    jmax_schema_ir::policy::validate_schema(schema, &options.policy)?;
    let canonical = jmax_schema_ir::canonical::canonicalize(schema, &options.policy)?;
    limited(count(canonical.bytes.len())?, options.max_image_bytes)?;
    let plans = plan_graph(schema, options)?;
    let slots = plan_slots(&plans)?;
    let (patterns, pattern_sources) = compile_patterns(&plans, options)?;
    let mut program = CompiledProgram {
        image: SchemaImage {
            header: SchemaImageHeader {
                magic: IMAGE_MAGIC,
                version: IMAGE_VERSION,
                flags: IMAGE_FLAGS,
                schema_id: schema.schema().schema_id.0,
                root_state: 0,
                insn_count: 0,
                key_count: 0,
                slot_count: count(slots.len())?,
                fingerprint: canonical.fingerprint,
            },
            insns: Vec::new(),
            keys: Vec::new(),
            key_bytes: Vec::new(),
        },
        states: Vec::new(),
        key_tables: Vec::new(),
        patterns,
        slots,
        policy: options.policy,
        format_options: FormatOptions::default(),
        canonical_schema: canonical.bytes,
    };
    reserve(&mut program.states, plans.len())?;
    for (state_index, plan) in plans.iter().enumerate() {
        let id = StateId(count(state_index)?);
        let first_insn = count(program.image.insns.len())?;
        let mut constraints = ConstraintSet::default();
        let kind = match plan.node {
            TypeNode::Any => {
                emit(&mut program.image.insns, options, Opcode::CheckScalarKind, 0, id.0, 255)?;
                StateKind::Any
            }
            TypeNode::Scalar(source) => {
                constraints = scalar_constraints(source, &pattern_sources)?;
                emit(&mut program.image.insns, options, Opcode::CheckScalarKind, 0, id.0, scalar_tag(source.kind))?;
                if constraints.min_i64.is_some() || constraints.max_i64.is_some() {
                    emit(&mut program.image.insns, options, Opcode::CheckIntBounds, 0, id.0, 0)?;
                }
                if source.kind == ValueKind::Decimal {
                    emit(&mut program.image.insns, options, Opcode::CheckDecimalBounds, 0, id.0, 0)?;
                }
                if constraints.min_count.is_some() || constraints.max_count.is_some() {
                    emit(&mut program.image.insns, options, Opcode::CheckStringLen, 0, id.0, 0)?;
                }
                if let Some(pattern) = constraints.pattern {
                    emit(&mut program.image.insns, options, Opcode::ExecPattern, 0, id.0, u64::from(pattern))?;
                }
                if let Some(format) = constraints.format {
                    emit(&mut program.image.insns, options, Opcode::ExecFormat, 0, id.0, u64::from(format as u16))?;
                }
                StateKind::Scalar(source.kind)
            }
            TypeNode::Enum(shape) => {
                constraints.nullable = shape.nullable;
                constraints.enum_variants.try_reserve_exact(shape.variants.len())
                    .map_err(|_| budget())?;
                for variant in &shape.variants {
                    let mut name = String::new();
                    name.try_reserve_exact(variant.name.len()).map_err(|_| budget())?;
                    name.push_str(&variant.name);
                    constraints.enum_variants.push(jmax_schema_ir::EnumVariant {
                        name, discriminant: variant.discriminant,
                    });
                }
                emit(&mut program.image.insns, options, Opcode::CheckEnum, 0, id.0, 0)?;
                StateKind::Enum
            }
            TypeNode::Object(shape) => {
                constraints.min_count = shape.min_properties;
                constraints.max_count = shape.max_properties;
                constraints.nullable = shape.nullable;
                let (table, required_count, match_opcode) = append_keys(&mut program, plan, options)?;
                emit(&mut program.image.insns, options, Opcode::EnterObject, 0, id.0, u64::from(table))?;
                emit(&mut program.image.insns, options, match_opcode, 0, table, 0)?;
                if shape.reject_unknown {
                    emit(&mut program.image.insns, options, Opcode::RejectUnknown, 0, id.0, 0)?;
                }
                for (property, target) in plan.properties.iter().zip(&plan.children) {
                    if let Some(slot) = property.slot {
                        let field_id = u16::try_from(property.field_id.0).map_err(|_| {
                            unsupported(property.field_id, "field identifier does not fit the image ABI")
                        })?;
                        emit(&mut program.image.insns, options, Opcode::EmitSlot, field_id, slot, u64::from(target.0))?;
                    }
                }
                emit(&mut program.image.insns, options, Opcode::LeaveObject, 0, id.0, 0)?;
                StateKind::Object { key_table: table, required_count, reject_unknown: shape.reject_unknown }
            }
            TypeNode::Array(shape) => {
                constraints.min_count = shape.min_items;
                constraints.max_count = shape.max_items;
                constraints.nullable = shape.nullable;
                let items = *plan.children.first().ok_or_else(|| invalid("array has no item state"))?;
                emit(&mut program.image.insns, options, Opcode::EnterArray, 0, id.0, u64::from(items.0))?;
                emit(&mut program.image.insns, options, Opcode::LeaveArray, 0, id.0, 0)?;
                StateKind::Array { items }
            }
            TypeNode::Ref(_) => return Err(invalid("resolved graph contains an uncollapsed alias")),
        };
        let len = count(program.image.insns.len())?.checked_sub(first_insn).ok_or_else(budget)?;
        program.states.push(ProgramState { id, kind, insns: Span32::new(first_insn, len), constraints });
    }
    program.image.header.insn_count = count(program.image.insns.len())?;
    program.image.header.key_count = count(program.image.keys.len())?;
    image::attach_schema_capsule(&mut program, options.max_image_bytes)?;
    let encoded_bytes = image::encoded_len(&program)?;
    limited(count(encoded_bytes)?, options.max_image_bytes)?;
    Ok(program)
}
