//! Versioned, portable fingerprint input for resolved schema graphs.
//!
//! Identity preserves stored constraint representations and graph sharing.
//! It ignores aliases, definitions, unreachable nodes, and declaration order.
//! FNV-1a is a cache hint, not evidence of equivalence or authenticity.

use super::*;

const PREFIX: &[u8; 12] = b"JMAX-SCHEMA\0";
// A graph has at most u32::MAX nodes, so its largest index is MAX - 1.
const UNDISCOVERED: u32 = u32::MAX;

fn error(id: SchemaId, field: Option<FieldId>, code: ErrorCode, detail: &'static str) -> Error {
    schema_error(code, detail).with_schema(id.0, field.map_or(0, |field| field.0))
}

fn count(value: usize, id: SchemaId, field: Option<FieldId>) -> Result<u32> {
    u32::try_from(value).map_err(|_| {
        error(id, field, ErrorCode::BudgetExhausted, "canonical count exceeds u32")
    })
}

fn reserve<T>(values: &mut Vec<T>, additional: usize, id: SchemaId) -> Result<()> {
    values.try_reserve(additional).map_err(|_| {
        error(id, None, ErrorCode::BudgetExhausted, "canonical allocation failed")
    })
}

/// The only byte sink: every growth is fallible, including capacity overflow.
struct Encoder {
    bytes: Vec<u8>,
    id: SchemaId,
    field: Option<FieldId>,
}

impl Encoder {
    fn new(id: SchemaId) -> Self {
        Self { bytes: Vec::new(), id, field: None }
    }

    fn raw(&mut self, bytes: &[u8]) -> Result<()> {
        self.bytes.try_reserve(bytes.len()).map_err(|_| {
            error(self.id, self.field, ErrorCode::BudgetExhausted,
                "canonical byte allocation failed")
        })?;
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }

    fn byte(&mut self, value: u8) -> Result<()> {
        self.raw(&[value])
    }

    fn boolean(&mut self, value: bool) -> Result<()> {
        self.byte(u8::from(value))
    }

    fn u32(&mut self, value: u32) -> Result<()> {
        self.raw(&value.to_le_bytes())
    }

    fn i64(&mut self, value: i64) -> Result<()> {
        self.raw(&value.to_le_bytes())
    }

    fn string(&mut self, value: &str) -> Result<()> {
        self.u32(count(value.len(), self.id, self.field)?)?;
        self.raw(value.as_bytes())
    }

    fn option<T>(&mut self, value: Option<T>, write: impl FnOnce(&mut Self, T) -> Result<()>)
        -> Result<()>
    {
        match value {
            None => self.byte(0),
            Some(value) => {
                self.byte(1)?;
                write(self, value)
            }
        }
    }

    fn policy(&mut self, policy: &SchemaPolicy) -> Result<()> {
        self.byte(match policy.dialect {
            Dialect::Endpoint => 0,
            Dialect::JsonSchema202012 => 1,
        })?;
        self.boolean(policy.numeric.allow_Quoted_int)?;
        self.boolean(policy.numeric.reject_non_finite_float)?;
        self.boolean(policy.numeric.reject_negative_zero)?;
        self.raw(&policy.numeric.max_exponent_abs.to_le_bytes())?;
        self.byte(match policy.unknown_formats {
            UnknownFormatPolicy::Reject => 0,
            UnknownFormatPolicy::Annotate => 1,
        })?;
        self.boolean(policy.allow_recursive)?;
        self.u32(policy.max_pattern_bytes)
    }

    fn decimal_value(&mut self, value: Decimal128) -> Result<()> {
        self.raw(&value.coefficient.to_le_bytes())?;
        self.byte(value.scale)
    }

    fn decimal(&mut self, value: &DecimalConstraints) -> Result<()> {
        self.byte(value.policy.max_precision)?;
        self.byte(value.policy.target_scale)?;
        self.boolean(value.policy.allow_exponent)?;
        self.boolean(value.policy.reject_inexact_rescale)?;
        self.raw(&value.policy.tick_step.to_le_bytes())?;
        self.option(value.minimum, Self::decimal_value)?;
        self.option(value.maximum, Self::decimal_value)
    }

    fn scalar(&mut self, value: &ScalarConstraints) -> Result<()> {
        self.byte(match value.kind {
            ValueKind::Null => 0,
            ValueKind::Boolean => 1,
            ValueKind::Integer => 2,
            ValueKind::Number => 3,
            ValueKind::Decimal => 4,
            ValueKind::String => 5,
        })?;
        self.option(value.min_i64, Self::i64)?;
        self.option(value.max_i64, Self::i64)?;
        self.option(value.decimal.as_ref(), Self::decimal)?;
        self.option(value.min_length, Self::u32)?;
        self.option(value.max_length, Self::u32)?;
        self.option(value.pattern.as_deref(), Self::string)?;
        self.option(value.format.as_deref(), Self::string)
    }
}

// Discovery occurs before processing. Each collapsed target enters the queue
// once, and previously assigned indices survive sharing and recursive edges.
fn discover(schema: &ResolvedSchema, ty: TypeId, field: Option<FieldId>,
    indices: &mut [u32], queue: &mut Vec<TypeId>) -> Result<u32>
{
    let id = schema.schema().schema_id;
    let target = resolve::resolve_type(schema, ty).map_err(|failure| {
        let field_id = field.map_or(failure.field_id, |field| field.0);
        failure.with_schema(id.0, field_id)
    })?;
    let index = indices.get_mut(target.0 as usize).ok_or_else(|| {
        error(id, field, ErrorCode::ConstraintViolation, "canonical target is out of bounds")
    })?;
    if *index == UNDISCOVERED {
        let next_count = queue.len().checked_add(1).ok_or_else(|| {
            error(id, field, ErrorCode::BudgetExhausted, "canonical node count overflow")
        })?;
        // Checking the count, not just the index, keeps MAX as the sentinel.
        let next_count = count(next_count, id, field)?;
        reserve(queue, 1, id)?;
        *index = next_count - 1;
        queue.push(target);
    }
    Ok(*index)
}

/// Encode canonical grammar v1 after validating the schema policy.
///
/// Nodes are numbered by breadth-first discovery from the collapsed root.
/// Object edges use ascending UTF-8 name bytes. Original type IDs, aliases,
/// definitions, and unreachable nodes never enter the byte stream. Explicit
/// slots, field IDs, bounds, schema metadata, and every policy field do.
///
/// Graph traversal is iterative. Auxiliary storage is O(stored nodes + largest
/// reachable object), and sorting costs O(p log p) per object with p properties.
/// All owned storage grows through fallible reservations. Neither input changes.
pub fn canonicalize(schema: &ResolvedSchema, policy: &SchemaPolicy) -> Result<CanonicalSchema> {
    policy::validate_schema(schema, policy)?;
    let stored = schema.schema();
    count(stored.nodes.len(), stored.schema_id, None)?;
    let mut indices = Vec::new();
    reserve(&mut indices, stored.nodes.len(), stored.schema_id)?;
    indices.resize(stored.nodes.len(), UNDISCOVERED);
    let mut queue = Vec::new();
    discover(schema, stored.root, None, &mut indices, &mut queue)?;
    let mut properties: Vec<&Property> = Vec::new();

    let mut encoder = Encoder::new(stored.schema_id);
    encoder.raw(PREFIX)?;
    encoder.raw(&CANONICAL_VERSION.to_le_bytes())?;
    encoder.policy(policy)?;
    encoder.string(&stored.name)?;
    encoder.u32(stored.schema_id.0)?;
    // Reserve the count's four bytes, then fill them once discovery completes.
    // No partially encoded output is observable if a later operation fails.
    let count_offset = encoder.bytes.len();
    encoder.u32(0)?;

    let mut next = 0;
    while next < queue.len() {
        let ty = queue[next];
        next += 1;
        encoder.field = None;
        // discover has checked this target and excluded Ref nodes.
        match &stored.nodes[ty.0 as usize] {
            TypeNode::Any => encoder.byte(0)?,
            TypeNode::Scalar(value) => {
                // Tags 4 to 6 are tags 1 to 3 of a node that also accepts null, so
                // every schema without a nullable node keeps its exact bytes.
                encoder.byte(if value.nullable { 4 } else { 1 })?;
                encoder.scalar(value)?;
            }
            TypeNode::Object(object) => {
                encoder.byte(if object.nullable { 5 } else { 2 })?;
                encoder.boolean(object.reject_unknown)?;
                encoder.option(object.min_properties, Encoder::u32)?;
                encoder.option(object.max_properties, Encoder::u32)?;
                encoder.u32(count(object.properties.len(), stored.schema_id, None)?)?;
                properties.clear();
                reserve(&mut properties, object.properties.len(), stored.schema_id)?;
                properties.extend(object.properties.iter());
                // Names are unique in the resolver's validated witness, so an
                // allocation-free unstable sort still has one exact ordering.
                properties.sort_unstable_by(|left, right| {
                    left.name.as_bytes().cmp(right.name.as_bytes())
                });
                for property in &properties {
                    encoder.field = Some(property.field_id);
                    let target = discover(schema, property.ty, encoder.field,
                        &mut indices, &mut queue)?;
                    encoder.u32(property.field_id.0)?;
                    encoder.string(&property.name)?;
                    encoder.u32(target)?;
                    encoder.boolean(property.required)?;
                    encoder.option(property.slot, Encoder::u32)?;
                }
            }
            TypeNode::Array(array) => {
                encoder.byte(if array.nullable { 6 } else { 3 })?;
                let target = discover(schema, array.items, None, &mut indices, &mut queue)?;
                encoder.u32(target)?;
                encoder.option(array.min_items, Encoder::u32)?;
                encoder.option(array.max_items, Encoder::u32)?;
            }
            TypeNode::Ref(_) => {
                return Err(error(stored.schema_id, None, ErrorCode::ConstraintViolation,
                    "canonical target must not be a reference"));
            }
        }
    }
    let reachable = count(queue.len(), stored.schema_id, None)?;
    encoder.bytes[count_offset..count_offset + 4].copy_from_slice(&reachable.to_le_bytes());
    let fingerprint = fingerprint_bytes(&encoder.bytes);
    Ok(CanonicalSchema { bytes: encoder.bytes, fingerprint })
}

/// Return exactly the fingerprint of `canonicalize(schema, policy)`.
/// Compare canonical bytes when hash collisions would affect correctness.
pub fn fingerprint(schema: &ResolvedSchema, policy: &SchemaPolicy) -> Result<u64> {
    Ok(canonicalize(schema, policy)?.fingerprint)
}

/// FNV-1a over precisely these bytes, with wrapping 64-bit multiplication.
/// This non-cryptographic function accepts any bytes, including an empty slice.
pub fn fingerprint_bytes(bytes: &[u8]) -> u64 {
    let mut fingerprint = Fingerprint::new();
    fingerprint.update(bytes);
    fingerprint.finish()
}
