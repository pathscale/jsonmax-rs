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
            TypeNode::Any | TypeNode::Scalar(_) => {}
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

fn plan_slots(plans: &[NodePlan<'_>]) -> Result<Vec<SlotSpec>> {
    let paths = path_counts(plans)?;
    let mut slots = Vec::new();
    for (owner, plan) in plans.iter().enumerate() {
        for (property, &state) in plan.properties.iter().zip(&plan.children) {
            if let Some(slot) = property.slot {
                if paths.get(owner).copied() != Some(1) {
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
        KeyStrategy::PerfectHash { slots, .. } => {
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
            TypeNode::Object(shape) => {
                constraints.min_count = shape.min_properties;
                constraints.max_count = shape.max_properties;
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
    let encoded_bytes = image::encoded_len(&program)?;
    limited(count(encoded_bytes)?, options.max_image_bytes)?;
    Ok(program)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{format, vec};
    use jmax_schema_ir::{
        ArrayShape, Decimal128, DecimalPolicy, NamedType, ResolveLimits, Schema,
        UnknownFormatPolicy,
    };

    fn property(name: &str, field: u32, ty: u32, required: bool, slot: Option<u32>) -> Property {
        Property { name: String::from(name), field_id: FieldId(field), ty: TypeId(ty), required, slot }
    }

    fn object(properties: Vec<Property>) -> TypeNode {
        TypeNode::Object(ObjectShape {
            properties, reject_unknown: false, min_properties: None, max_properties: None,
        })
    }

    fn array(items: u32) -> TypeNode {
        TypeNode::Array(ArrayShape { items: TypeId(items), min_items: None, max_items: None })
    }

    fn scalar(kind: ValueKind) -> TypeNode {
        TypeNode::Scalar(ScalarConstraints::new(kind))
    }

    fn schema(nodes: Vec<TypeNode>) -> Schema {
        Schema {
            name: String::from("lower-vectors"), schema_id: SchemaId(71), root: TypeId(0),
            nodes, definitions: Vec::new(),
        }
    }

    fn resolve(input: Schema) -> ResolvedSchema {
        jmax_schema_ir::resolve::resolve(input, ResolveLimits::default()).unwrap()
    }

    fn lower_nodes(nodes: Vec<TypeNode>) -> CompiledProgram {
        lower(&resolve(schema(nodes)), &CompilerOptions::default()).unwrap()
    }

    fn instructions(program: &CompiledProgram, state: usize) -> &[InsnWord] {
        let span = program.states[state].insns;
        let first = usize::try_from(span.offset).unwrap();
        let end = usize::try_from(span.end().unwrap()).unwrap();
        &program.image.insns[first..end]
    }

    fn opcodes(program: &CompiledProgram, state: usize) -> Vec<Opcode> {
        instructions(program, state).iter().map(|insn| decode_opcode(insn.opcode).unwrap()).collect()
    }

    fn assert_layout(program: &CompiledProgram) {
        let mut next = 0u32;
        for (id, state) in program.states.iter().enumerate() {
            assert_eq!(state.id, StateId(u32::try_from(id).unwrap()));
            assert_eq!(state.insns.offset, next);
            assert!(state.insns.len > 0);
            next = state.insns.end().unwrap();
        }
        assert_eq!(usize::try_from(next).unwrap(), program.image.insns.len());
        assert_eq!(program.image.header.insn_count, next);
        assert_eq!(usize::try_from(program.image.header.key_count).unwrap(), program.image.keys.len());
        assert_eq!(usize::try_from(program.image.header.slot_count).unwrap(), program.slots.len());
        for insn in &program.image.insns {
            assert_eq!(insn.flags, 0);
            let opcode = decode_opcode(insn.opcode).unwrap();
            if opcode != Opcode::EmitSlot {
                assert_eq!(insn.field_id, 0);
            }
            if matches!(opcode,
                Opcode::LeaveObject | Opcode::LeaveArray | Opcode::MatchKeySwar |
                Opcode::MatchKeyPhf | Opcode::RejectUnknown | Opcode::CheckIntBounds |
                Opcode::CheckDecimalBounds | Opcode::CheckStringLen
            ) {
                assert_eq!(insn.arg1, 0);
            }
        }
        assert_eq!(program.format_options, FormatOptions::default());
        assert_eq!(program.image.header.fingerprint,
            jmax_schema_ir::canonical::fingerprint_bytes(&program.canonical_schema));
    }

    fn error_for(nodes: Vec<TypeNode>, options: CompilerOptions, expected: ErrorCode) -> Error {
        let error = lower(&resolve(schema(nodes)), &options).unwrap_err();
        assert_eq!(error.code, expected);
        assert_eq!(error.source, ErrorSource::SchemaCompile);
        assert_eq!(error.byte_offset, 0);
        assert_eq!(error.schema_id, 71);
        error
    }

    #[test]
    fn any_has_one_explicit_instruction_and_exact_canonical_identity() {
        let resolved = resolve(schema(vec![TypeNode::Any]));
        let options = CompilerOptions::default();
        let program = lower(&resolved, &options).unwrap();
        let canonical = jmax_schema_ir::canonical::canonicalize(&resolved, &options.policy).unwrap();
        assert_eq!(program.canonical_schema, canonical.bytes);
        assert_eq!(program.image.header, SchemaImageHeader {
            magic: IMAGE_MAGIC, version: IMAGE_VERSION, flags: 0,
            schema_id: 71, root_state: 0, insn_count: 1, key_count: 0,
            slot_count: 0, fingerprint: canonical.fingerprint,
        });
        assert_eq!(program.image.insns, vec![InsnWord {
            opcode: opcode_byte(Opcode::CheckScalarKind), flags: 0, field_id: 0,
            arg0: 0, arg1: 255,
        }]);
        assert_eq!(program.states[0].kind, StateKind::Any);
        assert!(program.key_tables.is_empty());
        assert!(program.patterns.is_empty());
        assert!(program.slots.is_empty());
        assert_eq!(program.policy, options.policy);
        assert_layout(&program);
    }

    #[test]
    fn scalar_tags_and_bound_instructions_are_exact() {
        for (kind, tag) in [
            (ValueKind::Null, 0), (ValueKind::Boolean, 1), (ValueKind::Integer, 2),
            (ValueKind::Number, 3), (ValueKind::Decimal, 4), (ValueKind::String, 5),
        ] {
            let mut source = ScalarConstraints::new(kind);
            if kind == ValueKind::Integer {
                source.min_i64 = Some(i64::MIN);
                source.max_i64 = Some(i64::MAX);
            }
            if kind == ValueKind::Decimal {
                source.decimal = Some(DecimalConstraints {
                    policy: DecimalPolicy::spot_8(),
                    minimum: Some(Decimal128::new(-123, 8)),
                    maximum: Some(Decimal128::new(456, 8)),
                });
            }
            if kind == ValueKind::String {
                source.min_length = Some(0);
                source.max_length = Some(u32::MAX);
            }
            let program = lower_nodes(vec![TypeNode::Scalar(source.clone())]);
            assert_eq!(program.states[0].kind, StateKind::Scalar(kind));
            assert_eq!(instructions(&program, 0)[0].arg1, tag);
            let mut expected = vec![Opcode::CheckScalarKind];
            match kind {
                ValueKind::Integer => expected.push(Opcode::CheckIntBounds),
                ValueKind::Decimal => expected.push(Opcode::CheckDecimalBounds),
                ValueKind::String => expected.push(Opcode::CheckStringLen),
                _ => {}
            }
            assert_eq!(opcodes(&program, 0), expected);
            let constraints = &program.states[0].constraints;
            assert_eq!(constraints.min_i64, source.min_i64);
            assert_eq!(constraints.max_i64, source.max_i64);
            assert_eq!(constraints.decimal, source.decimal);
            assert_eq!(constraints.min_count, source.min_length);
            assert_eq!(constraints.max_count, source.max_length);
            assert_layout(&program);
        }
    }

    #[test]
    fn one_sided_bounds_emit_checks_and_absent_bounds_do_not() {
        for minimum in [false, true] {
            let mut integer = ScalarConstraints::new(ValueKind::Integer);
            let mut string = ScalarConstraints::new(ValueKind::String);
            if minimum {
                integer.min_i64 = Some(0);
                string.min_length = Some(0);
            } else {
                integer.max_i64 = Some(0);
                string.max_length = Some(0);
            }
            let program = lower_nodes(vec![TypeNode::Scalar(integer)]);
            assert_eq!(opcodes(&program, 0), [Opcode::CheckScalarKind, Opcode::CheckIntBounds]);
            let program = lower_nodes(vec![TypeNode::Scalar(string)]);
            assert_eq!(opcodes(&program, 0), [Opcode::CheckScalarKind, Opcode::CheckStringLen]);
        }
        for kind in [ValueKind::Integer, ValueKind::String] {
            assert_eq!(opcodes(&lower_nodes(vec![scalar(kind)]), 0), [Opcode::CheckScalarKind]);
        }
        let mut decimal = ScalarConstraints::new(ValueKind::Decimal);
        decimal.decimal = Some(DecimalConstraints {
            policy: DecimalPolicy::spot_8(), minimum: None, maximum: None,
        });
        assert_eq!(opcodes(&lower_nodes(vec![TypeNode::Scalar(decimal)]), 0),
            [Opcode::CheckScalarKind, Opcode::CheckDecimalBounds]);
    }

    #[test]
    fn object_keys_slots_and_required_bits_follow_sorted_names() {
        let root = ObjectShape {
            properties: vec![
                property("z", 8, 1, false, Some(0)),
                property("a", 9, 2, true, Some(1)),
                property("m", 10, 1, true, None),
            ],
            reject_unknown: true,
            min_properties: Some(2),
            max_properties: Some(3),
        };
        let program = lower_nodes(vec![TypeNode::Object(root), scalar(ValueKind::String), scalar(ValueKind::Integer)]);
        assert_eq!(program.states[0].kind, StateKind::Object {
            key_table: 0, required_count: 2, reject_unknown: true,
        });
        assert_eq!(program.states[0].constraints.min_count, Some(2));
        assert_eq!(program.states[0].constraints.max_count, Some(3));
        assert_eq!(opcodes(&program, 0), [
            Opcode::EnterObject, Opcode::MatchKeySwar, Opcode::RejectUnknown,
            Opcode::EmitSlot, Opcode::EmitSlot, Opcode::LeaveObject,
        ]);
        assert_eq!(program.image.key_bytes, b"amz");
        assert_eq!(program.image.keys.iter().map(|key| key.required_bit).collect::<Vec<_>>(),
            [0, 1, NO_REQUIRED_BIT]);
        assert_eq!(program.image.keys.iter().map(|key| key.field_id).collect::<Vec<_>>(), [9, 10, 8]);
        let emissions = &instructions(&program, 0)[3..5];
        assert_eq!((emissions[0].field_id, emissions[0].arg0, emissions[0].arg1), (9, 1, 1));
        assert_eq!((emissions[1].field_id, emissions[1].arg0, emissions[1].arg1), (8, 0, 2));
        assert_eq!(program.slots, [
            SlotSpec { slot: 0, field_id: FieldId(8), state: StateId(2) },
            SlotSpec { slot: 1, field_id: FieldId(9), state: StateId(1) },
        ]);
        assert_layout(&program);
    }

    #[test]
    fn empty_objects_keep_tables_and_zero_count_bounds() {
        let root = TypeNode::Object(ObjectShape {
            properties: Vec::new(), reject_unknown: true,
            min_properties: Some(0), max_properties: Some(0),
        });
        let program = lower_nodes(vec![root]);
        assert_eq!(program.key_tables, [KeyTable {
            first_key: 0, key_count: 0, strategy: KeyStrategy::Swar,
        }]);
        assert_eq!(opcodes(&program, 0), [
            Opcode::EnterObject, Opcode::MatchKeySwar, Opcode::RejectUnknown, Opcode::LeaveObject,
        ]);
        assert_eq!(program.states[0].constraints.min_count, Some(0));
        assert_eq!(program.states[0].constraints.max_count, Some(0));
        assert_layout(&program);
    }

    #[test]
    fn breadth_first_numbering_collapses_aliases_and_excludes_unreachable_nodes() {
        let mut input = schema(vec![
            TypeNode::Ref(String::from("root")),
            TypeNode::Any,
            object(vec![property("z", 1, 5, false, None), property("a", 2, 4, false, None)]),
            scalar(ValueKind::Null),
            array(6),
            TypeNode::Ref(String::from("text")),
            object(vec![property("child", 3, 5, false, None)]),
            scalar(ValueKind::String),
        ]);
        input.definitions = vec![
            NamedType { name: String::from("root"), ty: TypeId(2) },
            NamedType { name: String::from("text"), ty: TypeId(7) },
        ];
        let mut options = CompilerOptions::default();
        options.limits.max_states = 4;
        let program = lower(&resolve(input), &options).unwrap();
        assert_eq!(program.states.len(), 4);
        assert!(matches!(program.states[0].kind, StateKind::Object { key_table: 0, .. }));
        assert_eq!(program.states[1].kind, StateKind::Array { items: StateId(3) });
        assert_eq!(program.states[2].kind, StateKind::Scalar(ValueKind::String));
        assert!(matches!(program.states[3].kind, StateKind::Object { key_table: 1, .. }));
        assert_eq!(program.image.keys.iter().map(|key| key.target_state).collect::<Vec<_>>(), [1, 2, 2]);
        assert_layout(&program);
    }

    #[test]
    fn array_grammar_preserves_item_bounds_and_shared_child_state() {
        let program = lower_nodes(vec![
            TypeNode::Array(ArrayShape { items: TypeId(1), min_items: Some(0), max_items: Some(u32::MAX) }),
            TypeNode::Any,
        ]);
        assert_eq!(opcodes(&program, 0), [Opcode::EnterArray, Opcode::LeaveArray]);
        assert_eq!(instructions(&program, 0)[0].arg1, 1);
        assert_eq!(program.states[0].constraints.min_count, Some(0));
        assert_eq!(program.states[0].constraints.max_count, Some(u32::MAX));
        assert_layout(&program);
    }

    #[test]
    fn object_and_array_recursion_remain_finite_state_edges() {
        let object_program = lower_nodes(vec![object(vec![property("next", 0, 0, false, None)])]);
        assert_eq!(object_program.states.len(), 1);
        assert_eq!(object_program.image.keys[0].target_state, 0);
        assert_layout(&object_program);
        let array_program = lower_nodes(vec![array(0)]);
        assert_eq!(array_program.states.len(), 1);
        assert_eq!(array_program.states[0].kind, StateKind::Array { items: StateId(0) });
        assert_layout(&array_program);
    }

    #[test]
    fn repeated_owner_paths_reject_slots_with_field_context() {
        let cases = vec![
            vec![array(1), object(vec![property("value", 99, 2, false, Some(0))]), TypeNode::Any],
            vec![object(vec![property("next", 99, 0, false, Some(0))])],
            vec![
                object(vec![property("a", 1, 1, false, None), property("b", 2, 1, false, None)]),
                object(vec![property("value", 99, 2, false, Some(0))]), TypeNode::Any,
            ],
            // The selected property's owner is below, rather than in, a cycle.
            vec![
                object(vec![property("next", 1, 0, false, None), property("leaf", 2, 1, false, None)]),
                object(vec![property("value", 99, 2, false, Some(0))]), TypeNode::Any,
            ],
            // Distinct paths merge at different depths; count propagation must
            // not depend on the order in which the owner is first discovered.
            vec![
                object(vec![property("a", 1, 2, false, None), property("b", 2, 1, false, None)]),
                object(vec![property("via", 3, 2, false, None)]),
                object(vec![property("value", 99, 3, false, Some(0))]), TypeNode::Any,
            ],
        ];
        for nodes in cases {
            let error = error_for(nodes, CompilerOptions::default(), ErrorCode::UnsupportedSchemaFeature);
            assert_eq!(error.field_id, 99);
        }
    }

    #[test]
    fn unique_container_captures_allow_repetition_inside_the_captured_value() {
        let program = lower_nodes(vec![
            object(vec![
                property("array", 1, 1, false, Some(2)),
                property("object", 2, 2, false, Some(0)),
                property("any", 3, 3, false, Some(1)),
            ]),
            array(3),
            object(vec![property("next", 4, 2, false, None)]),
            TypeNode::Any,
        ]);
        assert_eq!(program.slots.len(), 3);
        assert_eq!(program.slots[0].field_id, FieldId(2));
        assert_eq!(program.slots[1].field_id, FieldId(3));
        assert_eq!(program.slots[2].field_id, FieldId(1));
        assert_eq!(program.states[usize::try_from(program.slots[2].state.0).unwrap()].kind,
            StateKind::Array { items: StateId(1) });
        assert_layout(&program);
    }

    #[test]
    fn reused_scalar_types_allow_distinct_field_occurrence_slots() {
        let program = lower_nodes(vec![
            object(vec![property("a", 17, 1, false, Some(1)), property("b", 18, 1, false, Some(0))]),
            scalar(ValueKind::Integer),
        ]);
        assert_eq!(program.states.len(), 2);
        assert_eq!(program.slots, [
            SlotSpec { slot: 0, field_id: FieldId(18), state: StateId(1) },
            SlotSpec { slot: 1, field_id: FieldId(17), state: StateId(1) },
        ]);
    }

    #[test]
    fn reused_object_types_keep_one_table_and_globally_unique_field_ids() {
        let program = lower_nodes(vec![
            object(vec![property("a", 17, 1, false, Some(0)), property("b", 18, 1, false, Some(1))]),
            object(vec![property("inner", 19, 2, true, None)]), TypeNode::Any,
        ]);
        assert_eq!(program.states.len(), 3);
        assert_eq!(program.key_tables.len(), 2);
        assert_eq!(program.image.header.key_count, 3);
        assert_eq!(program.image.keys.iter().map(|key| key.field_id).collect::<Vec<_>>(), [17, 18, 19]);
        assert_eq!(program.slots[0].state, StateId(1));
        assert_eq!(program.slots[1].state, StateId(1));
        assert_layout(&program);
    }

    #[test]
    fn slot_holes_and_unreachable_slot_holes_are_not_renumbered() {
        for slot in [1, 2, u32::MAX] {
            let error = error_for(vec![
                object(vec![property("selected", 42, 1, false, Some(slot))]), TypeNode::Any,
            ], CompilerOptions::default(), ErrorCode::ConstraintViolation);
            assert_eq!(error.field_id, 42);
        }
        error_for(vec![
            object(vec![property("selected", 42, 1, false, Some(1))]), TypeNode::Any,
            object(vec![property("unreachable", 43, 1, false, Some(0))]),
        ], CompilerOptions::default(), ErrorCode::ConstraintViolation);
        let program = lower_nodes(vec![TypeNode::Any,
            object(vec![property("unreachable", 43, 0, false, Some(7))]),
        ]);
        assert!(program.slots.is_empty());
    }

    #[test]
    fn path_counts_saturate_and_do_not_overcount_a_single_chain() {
        let resolved = resolve(schema(vec![
            object(vec![property("a", 1, 1, false, None), property("b", 2, 1, false, None)]),
            object(vec![property("c", 3, 2, false, None)]), array(3), TypeNode::Any,
        ]));
        let plans = plan_graph(&resolved, &CompilerOptions::default()).unwrap();
        assert_eq!(path_counts(&plans).unwrap(), [1, 2, 2, 2]);
        let resolved = resolve(schema(vec![
            object(vec![property("a", 1, 1, false, None)]),
            object(vec![property("b", 2, 2, false, Some(0))]), TypeNode::Any,
        ]));
        let plans = plan_graph(&resolved, &CompilerOptions::default()).unwrap();
        assert_eq!(path_counts(&plans).unwrap(), [1, 1, 1]);
        assert!(lower(&resolved, &CompilerOptions::default()).is_ok());
    }

    #[test]
    fn key_bytes_and_perfect_hash_indices_are_rebased_once() {
        let resolved = resolve(schema(vec![
            object(vec![property("outer", 1, 1, false, None)]),
            object(vec![property("é", 2, 2, true, None)]),
            object(Vec::new()),
        ]));
        let mut options = CompilerOptions::default();
        options.swar_threshold = 0;
        let program = lower(&resolved, &options).unwrap();
        assert_eq!(program.image.key_bytes, "outeré".as_bytes());
        assert_eq!(program.image.keys[0].span, Span32::new(0, 5));
        assert_eq!(program.image.keys[1].span, Span32::new(5, 2));
        assert_eq!(program.key_tables.iter().map(|table| table.first_key).collect::<Vec<_>>(), [0, 1, 2]);
        assert_eq!(program.key_tables[2].strategy, KeyStrategy::Swar);
        for table_index in 0..2 {
            let table = &program.key_tables[table_index];
            let name = if table_index == 0 { "outer" } else { "é" };
            match &table.strategy {
                KeyStrategy::PerfectHash { slots, .. } => {
                    assert_eq!(slots.iter().copied().filter(|&slot| slot != EMPTY_KEY_SLOT).collect::<Vec<_>>(),
                        [u32::try_from(table_index).unwrap()]);
                }
                KeyStrategy::Swar => panic!("one key must fit a perfect hash with the default attempt budget"),
            }
            assert_eq!(opcodes(&program, table_index)[1], Opcode::MatchKeyPhf);
            assert_eq!(instructions(&program, table_index)[1].arg0, u32::try_from(table_index).unwrap());
            assert_eq!(keys::lookup(&program.image, table, name.as_bytes()).unwrap().unwrap().span,
                program.image.keys[table_index].span);
            keys::validate(&program.image, table).unwrap();
        }
        assert_layout(&program);
    }

    #[test]
    fn empty_and_prefix_keys_are_preserved_without_padding() {
        let program = lower_nodes(vec![object(vec![
            property("abcdefghx", 3, 1, false, None), property("", 0, 1, true, None),
            property("abcdefgh", 2, 1, false, None), property("a", 1, 1, true, None),
        ]), TypeNode::Any]);
        assert_eq!(program.image.key_bytes, b"aabcdefghabcdefghx");
        assert_eq!(program.image.keys[0].span, Span32::new(0, 0));
        let table = &program.key_tables[0];
        for (name, field_id) in [("", 0), ("a", 1), ("abcdefgh", 2), ("abcdefghx", 3)] {
            assert_eq!(keys::lookup(&program.image, table, name.as_bytes()).unwrap().unwrap().field_id, field_id);
        }
        assert!(keys::lookup(&program.image, table, b"abcdefghz").unwrap().is_none());
    }

    #[test]
    fn no_hash_attempts_produces_swar_without_changing_key_semantics() {
        let mut options = CompilerOptions::default();
        options.swar_threshold = 0;
        options.max_hash_attempts = 0;
        let program = lower(&resolve(schema(vec![object(vec![property("a", 0, 1, false, None)]), TypeNode::Any])), &options).unwrap();
        assert_eq!(program.key_tables[0].strategy, KeyStrategy::Swar);
        assert_eq!(opcodes(&program, 0)[1], Opcode::MatchKeySwar);
        assert_eq!(program.image.key_bytes, b"a");
    }

    fn patterned(source: &str) -> TypeNode {
        let mut constraints = ScalarConstraints::new(ValueKind::String);
        constraints.pattern = Some(String::from(source));
        TypeNode::Scalar(constraints)
    }

    #[test]
    fn pattern_order_is_first_state_encounter_with_identical_text_deduplicated() {
        let program = lower_nodes(vec![
            object(vec![property("c", 3, 3, false, None), property("a", 1, 1, false, None),
                property("b", 2, 2, false, None)]),
            patterned("z"), patterned("a"), patterned("z"),
        ]);
        assert_eq!(program.patterns.len(), 2);
        let options = CompilerOptions::default();
        assert_eq!(program.patterns[0], patterns::compile("z", options.limits.max_pattern_states, options.max_pattern_steps).unwrap());
        assert_eq!(program.patterns[1], patterns::compile("a", options.limits.max_pattern_states, options.max_pattern_steps).unwrap());
        assert_eq!(program.states[1].constraints.pattern, Some(0));
        assert_eq!(program.states[2].constraints.pattern, Some(1));
        assert_eq!(program.states[3].constraints.pattern, Some(0));
        for state in 1..4 {
            assert_eq!(opcodes(&program, state), [Opcode::CheckScalarKind, Opcode::ExecPattern]);
        }
        assert_layout(&program);
    }

    #[test]
    fn pattern_total_limit_is_inclusive_and_counts_only_unique_sources() {
        let nodes = vec![object(vec![property("a", 1, 1, false, None), property("b", 2, 2, false, None)]),
            patterned("a"), patterned("b")];
        let resolved = resolve(schema(nodes.clone()));
        let baseline = lower(&resolved, &CompilerOptions::default()).unwrap();
        let total = baseline.patterns.iter().map(|p| u32::try_from(p.ops.len()).unwrap()).sum::<u32>();
        let mut options = CompilerOptions::default();
        options.limits.max_pattern_states = total;
        assert!(lower(&resolved, &options).is_ok());
        options.limits.max_pattern_states = total - 1;
        error_for(nodes, options, ErrorCode::BudgetExhausted);
        options.limits.max_pattern_states = u32::try_from(baseline.patterns[0].ops.len()).unwrap();
        assert!(lower(&resolve(schema(vec![
            object(vec![property("a", 1, 1, false, None), property("b", 2, 2, false, None)]),
            patterned("a"), patterned("a"),
        ])), &options).is_ok());
        options.limits.max_pattern_states = 0;
        error_for(vec![patterned("")], options, ErrorCode::BudgetExhausted);
    }

    #[test]
    fn pattern_syntax_errors_preserve_compile_error_context() {
        error_for(vec![patterned("(")], CompilerOptions::default(), ErrorCode::ConstraintViolation);
        error_for(vec![patterned("\\d")], CompilerOptions::default(), ErrorCode::UnsupportedSchemaFeature);
        let mut options = CompilerOptions::default();
        options.policy.max_pattern_bytes = 1;
        error_for(vec![patterned("é")], options, ErrorCode::BudgetExhausted);
    }

    #[test]
    fn string_checks_are_emitted_in_length_pattern_format_order() {
        let mut source = ScalarConstraints::new(ValueKind::String);
        source.min_length = Some(1);
        source.pattern = Some(String::from("."));
        source.format = Some(String::from("uuid"));
        let program = lower_nodes(vec![TypeNode::Scalar(source)]);
        assert_eq!(opcodes(&program, 0), [Opcode::CheckScalarKind, Opcode::CheckStringLen,
            Opcode::ExecPattern, Opcode::ExecFormat]);
        assert_eq!(instructions(&program, 0)[2].arg1, 0);
        assert_eq!(instructions(&program, 0)[3].arg1, 1);
        assert_eq!(program.states[0].constraints.format, Some(FormatId::Uuid));
        assert_layout(&program);
    }

    #[test]
    fn every_registered_format_uses_its_stable_identifier() {
        for number in 1u16..=10 {
            let id = FormatId::from_u16(number).unwrap();
            let mut source = ScalarConstraints::new(ValueKind::String);
            source.format = Some(String::from(id.name()));
            let program = lower_nodes(vec![TypeNode::Scalar(source)]);
            assert_eq!(program.states[0].constraints.format, Some(id));
            assert_eq!(opcodes(&program, 0), [Opcode::CheckScalarKind, Opcode::ExecFormat]);
            assert_eq!(instructions(&program, 0)[1].arg1, u64::from(number));
        }
    }

    #[test]
    fn unknown_formats_are_annotations_only_when_policy_allows_it() {
        for name in ["", "UUID", "custom-format", "an-unknown-format-name-longer-than-the-registrys-sixty-four-byte-name-limit"] {
            let mut source = ScalarConstraints::new(ValueKind::String);
            source.format = Some(String::from(name));
            error_for(vec![TypeNode::Scalar(source.clone())], CompilerOptions::default(), ErrorCode::UnsupportedSchemaFeature);
            let mut options = CompilerOptions::default();
            options.policy.unknown_formats = UnknownFormatPolicy::Annotate;
            let resolved = resolve(schema(vec![TypeNode::Scalar(source)]));
            let program = lower(&resolved, &options).unwrap();
            assert_eq!(program.states[0].constraints.format, None);
            assert_eq!(opcodes(&program, 0), [Opcode::CheckScalarKind]);
            assert_eq!(program.canonical_schema,
                jmax_schema_ir::canonical::canonicalize(&resolved, &options.policy).unwrap().bytes);
            let unannotated = lower(&resolve(schema(vec![scalar(ValueKind::String)])), &options).unwrap();
            assert_ne!(program.canonical_schema, unannotated.canonical_schema);
        }
    }

    #[test]
    fn global_state_instruction_key_and_image_limits_are_inclusive() {
        let resolved = resolve(schema(vec![
            object(vec![property("a", 0, 1, false, None)]),
            object(vec![property("b", 1, 2, false, None)]), TypeNode::Any,
        ]));
        let baseline = lower(&resolved, &CompilerOptions::default()).unwrap();
        let mut exact = CompilerOptions::default();
        exact.limits.max_states = u32::try_from(baseline.states.len()).unwrap();
        exact.limits.max_insns = baseline.image.header.insn_count;
        exact.limits.max_keys = baseline.image.header.key_count;
        exact.max_image_bytes = u32::try_from(image::encoded_len(&baseline).unwrap()).unwrap();
        assert!(lower(&resolved, &exact).is_ok());
        for component in 0..4 {
            let mut options = exact;
            match component {
                0 => options.limits.max_states -= 1,
                1 => options.limits.max_insns -= 1,
                2 => options.limits.max_keys -= 1,
                _ => options.max_image_bytes -= 1,
            }
            let error = lower(&resolved, &options).unwrap_err();
            assert_eq!(error.code, ErrorCode::BudgetExhausted);
            assert_eq!(error.schema_id, 71);
        }
    }

    #[test]
    fn zero_limits_only_reject_resources_that_are_needed() {
        let mut options = CompilerOptions::default();
        options.limits.max_keys = 0;
        options.limits.max_pattern_states = 0;
        options.max_pattern_steps = 0;
        assert!(lower(&resolve(schema(vec![object(Vec::new())])), &options).is_ok());
        options.limits.max_states = 0;
        error_for(vec![TypeNode::Any], options, ErrorCode::BudgetExhausted);
        options.limits.max_states = 1;
        options.limits.max_insns = 0;
        error_for(vec![TypeNode::Any], options, ErrorCode::BudgetExhausted);
        options.limits.max_insns = 1;
        assert!(lower(&resolve(schema(vec![TypeNode::Any])), &options).is_ok());
        options.max_image_bytes = 0;
        error_for(vec![TypeNode::Any], options, ErrorCode::BudgetExhausted);
    }

    #[test]
    fn policy_is_validated_before_executable_resource_limits() {
        let mut options = CompilerOptions::default();
        options.policy.allow_recursive = false;
        options.limits.max_states = 0;
        error_for(vec![array(0)], options, ErrorCode::UnsupportedSchemaFeature);
        options.policy.max_pattern_bytes = 0;
        error_for(vec![TypeNode::Any], options, ErrorCode::ConstraintViolation);
    }

    #[test]
    fn field_identifier_boundary_is_inclusive() {
        let program = lower_nodes(vec![
            object(vec![property("last", u32::from(u16::MAX), 1, true, Some(0))]), TypeNode::Any,
        ]);
        assert_eq!(program.image.keys[0].field_id, u16::MAX);
        assert_eq!(program.image.keys[0].required_bit, 0);
        assert_eq!(instructions(&program, 0)[2].field_id, u16::MAX);
        for field in [u32::from(u16::MAX) + 1, u32::MAX] {
            let error = error_for(vec![object(vec![property("bad", field, 1, false, None)]), TypeNode::Any],
                CompilerOptions::default(), ErrorCode::UnsupportedSchemaFeature);
            assert_eq!(error.field_id, field);
        }
    }

    #[test]
    fn required_bit_sentinel_allows_65535_required_but_not_65536() {
        let mut properties = Vec::new();
        for field in 0..u32::from(u16::MAX) {
            properties.push(property(&format!("p{field:05}"), field, 1, true, None));
        }
        let mut options = CompilerOptions::default();
        options.swar_threshold = u16::MAX;
        options.max_hash_attempts = 0;
        let program = lower(&resolve(schema(vec![object(properties.clone()), TypeNode::Any])), &options).unwrap();
        assert!(matches!(program.states[0].kind, StateKind::Object { required_count: u16::MAX, .. }));
        assert_eq!(program.image.keys.last().unwrap().required_bit, u16::MAX - 1);
        properties.push(property("p65535", u32::from(u16::MAX), 1, true, None));
        let error = error_for(vec![object(properties.clone()), TypeNode::Any], options, ErrorCode::UnsupportedSchemaFeature);
        assert_eq!(error.field_id, u32::from(u16::MAX));
        properties.last_mut().unwrap().required = false;
        let program = lower(&resolve(schema(vec![object(properties), TypeNode::Any])), &options).unwrap();
        assert_eq!(program.image.header.key_count, 65_536);
        assert_eq!(program.image.keys.last().unwrap().required_bit, NO_REQUIRED_BIT);
        assert_layout(&program);
    }

    #[test]
    fn arithmetic_and_malformed_internal_edges_return_errors() {
        assert_eq!(add(u32::MAX, 0).unwrap(), u32::MAX);
        assert_eq!(add(u32::MAX, 1).unwrap_err().code, ErrorCode::BudgetExhausted);
        assert!(limited(7, 7).is_ok());
        assert_eq!(limited(8, 7).unwrap_err().code, ErrorCode::BudgetExhausted);
        assert_eq!(path_counts(&[]).unwrap_err().code, ErrorCode::ConstraintViolation);
        let node = array(0);
        let plans = [NodePlan { node: &node, properties: Vec::new(), children: vec![StateId(1)] }];
        assert_eq!(path_counts(&plans).unwrap_err().code, ErrorCode::ConstraintViolation);
        if let Ok(too_many) = usize::try_from(u64::from(u32::MAX) + 1) {
            assert_eq!(count(too_many).unwrap_err().code, ErrorCode::BudgetExhausted);
        }
    }

    #[test]
    fn long_graphs_use_iterative_discovery_and_capture_analysis() {
        let mut nodes = Vec::new();
        for ty in 0u32..1024 {
            nodes.push(array(ty + 1));
        }
        nodes.push(TypeNode::Any);
        let program = lower_nodes(nodes);
        assert_eq!(program.states.len(), 1025);
        assert_eq!(program.image.header.insn_count, 2049);
        assert_layout(&program);
    }

    #[test]
    fn reordered_properties_produce_identical_validated_encodings() {
        let mut first = schema(vec![
            object(vec![property("z", 1, 1, false, Some(0)), property("a", 2, 1, true, Some(1))]),
            scalar(ValueKind::String),
        ]);
        let options = CompilerOptions::default();
        let before = compile(&resolve(first.clone()), &options).unwrap();
        if let TypeNode::Object(shape) = &mut first.nodes[0] {
            shape.properties.reverse();
        }
        let after = compile(&resolve(first), &options).unwrap();
        let before_bytes = image::encode(&before, options.max_image_bytes).unwrap();
        let after_bytes = image::encode(&after, options.max_image_bytes).unwrap();
        assert_eq!(before_bytes, after_bytes);
        assert_eq!(image::encoded_len(before.program()).unwrap(), before_bytes.len());
        assert_layout(before.program());
        assert_layout(after.program());
    }
}
