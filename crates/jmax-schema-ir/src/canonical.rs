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
                encoder.byte(1)?;
                encoder.scalar(value)?;
            }
            TypeNode::Object(object) => {
                encoder.byte(2)?;
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
                encoder.byte(3)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{format, vec};

    fn graph(nodes: Vec<TypeNode>) -> Schema {
        Schema {
            name: String::new(), schema_id: SchemaId(0), root: TypeId(0),
            nodes, definitions: Vec::new(),
        }
    }

    fn resolved(schema: Schema) -> ResolvedSchema {
        resolve::resolve(schema, ResolveLimits::default()).unwrap()
    }

    fn canonical(schema: Schema) -> CanonicalSchema {
        canonicalize(&resolved(schema), &SchemaPolicy::default()).unwrap()
    }

    fn property(id: u32, name: &str, ty: u32) -> Property {
        Property {
            field_id: FieldId(id), name: String::from(name), ty: TypeId(ty),
            required: false, slot: None,
        }
    }

    fn object(properties: Vec<Property>) -> TypeNode {
        TypeNode::Object(ObjectShape {
            properties, reject_unknown: true, min_properties: None, max_properties: None,
        })
    }

    fn array(items: u32) -> TypeNode {
        TypeNode::Array(ArrayShape { items: TypeId(items), min_items: None, max_items: None })
    }

    fn scalar(kind: ValueKind) -> TypeNode {
        TypeNode::Scalar(ScalarConstraints::new(kind))
    }

    fn text(pattern: Option<&str>, format: Option<&str>) -> TypeNode {
        let mut value = ScalarConstraints::new(ValueKind::String);
        value.pattern = pattern.map(String::from);
        value.format = format.map(String::from);
        TypeNode::Scalar(value)
    }

    fn decimal(minimum: Option<Decimal128>, maximum: Option<Decimal128>) -> TypeNode {
        let mut value = ScalarConstraints::new(ValueKind::Decimal);
        value.decimal = Some(DecimalConstraints {
            policy: DecimalPolicy::spot_8(), minimum, maximum,
        });
        TypeNode::Scalar(value)
    }

    // Literal grammar v1 prefix and default policy, followed by an empty schema
    // name, schema ID zero, and the supplied reachable count. Node vectors below
    // are independent of Encoder and pin field order, widths, and option tags.
    fn expected(nodes: u32, records: &[u8]) -> Vec<u8> {
        let mut bytes = vec![
            0x4a, 0x4d, 0x41, 0x58, 0x2d, 0x53, 0x43, 0x48, 0x45, 0x4d, 0x41, 0,
            1, 0,
            0, 0, 1, 1, 0x34, 1, 0, 1, 0, 0x10, 0, 0,
            0, 0, 0, 0,
            0, 0, 0, 0,
        ];
        bytes.extend_from_slice(&nodes.to_le_bytes());
        bytes.extend_from_slice(records);
        bytes
    }

    fn assert_vector(schema: Schema, nodes: u32, records: &[u8]) {
        let schema = resolved(schema);
        let before = schema.schema().clone();
        let policy = SchemaPolicy::default();
        let result = canonicalize(&schema, &policy).unwrap();
        assert_eq!(result.bytes, expected(nodes, records));
        assert_eq!(result.fingerprint, fingerprint_bytes(&result.bytes));
        assert_eq!(fingerprint(&schema, &policy).unwrap(), result.fingerprint);
        assert_eq!(canonicalize(&schema, &policy).unwrap(), result);
        assert_eq!(schema.schema(), &before);
        assert_eq!(policy, SchemaPolicy::default());
    }

    fn assert_error(failure: Error, code: ErrorCode, schema: u32, field: u32) {
        assert_eq!(failure.code, code);
        assert_eq!(failure.source, ErrorSource::SchemaCompile);
        assert_eq!(failure.byte_offset, 0);
        assert_eq!(failure.schema_id, schema);
        assert_eq!(failure.field_id, field);
        assert!(!failure.detail.is_empty());
    }

    #[test]
    fn fnv1a_known_vectors_and_incremental_partitions() {
        let vectors: &[(&[u8], u64)] = &[
            (b"", 0xcbf29ce484222325),
            (b"a", 0xaf63dc4c8601ec8c),
            (b"b", 0xaf63df4c8601f1a5),
            (b"foobar", 0x85944171f73967e8),
            (b"hello", 0xa430d84680aabd0b),
        ];
        for &(bytes, hash) in vectors {
            assert_eq!(fingerprint_bytes(bytes), hash);
            for split in 0..=bytes.len() {
                let mut incremental = Fingerprint::new();
                incremental.update(&bytes[..split]);
                incremental.update(b"");
                incremental.update(&bytes[split..]);
                assert_eq!(incremental.finish(), hash);
            }
        }
        let binary: Vec<u8> = (0..=255).collect();
        let mut incremental = Fingerprint::new();
        for byte in &binary {
            incremental.update(&[*byte]);
        }
        assert_eq!(fingerprint_bytes(&binary), incremental.finish());
        assert_ne!(fingerprint_bytes(b"a\0b"), fingerprint_bytes(b"ab"));
        assert_ne!(fingerprint_bytes(b"ab"), fingerprint_bytes(b"ba"));
    }

    #[test]
    fn any_empty_object_and_all_scalar_kind_tags() {
        assert_eq!(CANONICAL_VERSION, 1);
        assert_vector(graph(vec![TypeNode::Any]), 1, &[0]);
        assert_vector(graph(vec![object(vec![])]), 1, &[2, 1, 0, 0, 0, 0, 0, 0]);
        for (kind, tag) in [
            (ValueKind::Null, 0), (ValueKind::Boolean, 1),
            (ValueKind::Integer, 2), (ValueKind::Number, 3), (ValueKind::String, 5),
        ] {
            assert_vector(graph(vec![scalar(kind)]), 1, &[1, tag, 0, 0, 0, 0, 0, 0, 0]);
        }
        assert_vector(graph(vec![decimal(None, None)]), 1, &[
            1, 4, 0, 0, 1,
            38, 8, 0, 1,
            1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0,
        ]);
    }

    #[test]
    fn integer_extremes_are_signed_little_endian() {
        let mut value = ScalarConstraints::new(ValueKind::Integer);
        value.min_i64 = Some(i64::MIN);
        value.max_i64 = Some(i64::MAX);
        assert_vector(graph(vec![TypeNode::Scalar(value)]), 1, &[
            1, 2,
            1, 0, 0, 0, 0, 0, 0, 0, 0x80,
            1, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x7f,
            0, 0, 0, 0, 0,
        ]);
    }

    #[test]
    fn decimal_extremes_and_every_decimal_policy_field_have_exact_bytes() {
        let mut value = ScalarConstraints::new(ValueKind::Decimal);
        value.decimal = Some(DecimalConstraints {
            policy: DecimalPolicy {
                max_precision: 38, target_scale: 38, allow_exponent: true,
                reject_inexact_rescale: false, tick_step: i128::MAX,
            },
            minimum: Some(Decimal128::new(i128::MIN, 38)),
            maximum: Some(Decimal128::new(i128::MAX, 38)),
        });
        assert_vector(graph(vec![TypeNode::Scalar(value)]), 1, &[
            1, 4, 0, 0, 1, 38, 38, 1, 0,
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x7f,
            1,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x80, 38,
            1,
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x7f, 38,
            0, 0, 0, 0,
        ]);
    }

    #[test]
    fn string_lengths_patterns_and_formats_use_utf8_byte_lengths() {
        let mut value = ScalarConstraints::new(ValueKind::String);
        value.min_length = Some(0);
        value.max_length = Some(u32::MAX);
        value.pattern = Some(String::from("é\0"));
        value.format = Some(String::from("hex"));
        assert_vector(graph(vec![TypeNode::Scalar(value)]), 1, &[
            1, 5, 0, 0, 0,
            1, 0, 0, 0, 0,
            1, 0xff, 0xff, 0xff, 0xff,
            1, 3, 0, 0, 0, 0xc3, 0xa9, 0,
            1, 3, 0, 0, 0, b'h', b'e', b'x',
        ]);
        assert_vector(graph(vec![text(Some(""), None)]), 1, &[
            1, 5, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0,
        ]);
    }

    #[test]
    fn object_property_and_array_boundaries_have_exact_records() {
        let mut field = property(u32::MAX, "", 1);
        field.required = true;
        field.slot = Some(u32::MAX);
        assert_vector(graph(vec![
            TypeNode::Object(ObjectShape {
                properties: vec![field], reject_unknown: false,
                min_properties: Some(0), max_properties: Some(u32::MAX),
            }),
            TypeNode::Array(ArrayShape {
                items: TypeId(2), min_items: Some(0), max_items: Some(u32::MAX),
            }), TypeNode::Any,
        ]), 3, &[
            2, 0, 1, 0, 0, 0, 0, 1, 0xff, 0xff, 0xff, 0xff,
            1, 0, 0, 0,
            0xff, 0xff, 0xff, 0xff, 0, 0, 0, 0, 1, 0, 0, 0, 1,
            1, 0xff, 0xff, 0xff, 0xff,
            3, 2, 0, 0, 0, 1, 0, 0, 0, 0, 1, 0xff, 0xff, 0xff, 0xff,
            0,
        ]);
    }

    #[test]
    fn breadth_first_numbering_is_not_depth_first_or_storage_order() {
        let input = graph(vec![
            object(vec![property(20, "b", 3), property(10, "a", 1)]),
            array(2), scalar(ValueKind::String), scalar(ValueKind::Boolean),
        ]);
        assert_vector(input, 4, &[
            2, 1, 0, 0, 2, 0, 0, 0,
            10, 0, 0, 0, 1, 0, 0, 0, b'a', 1, 0, 0, 0, 0, 0,
            20, 0, 0, 0, 1, 0, 0, 0, b'b', 2, 0, 0, 0, 0, 0,
            3, 3, 0, 0, 0, 0, 0,
            1, 1, 0, 0, 0, 0, 0, 0, 0,
            1, 5, 0, 0, 0, 0, 0, 0, 0,
        ]);
    }

    #[test]
    fn reordering_storage_and_renaming_aliases_and_definitions_preserves_identity() {
        let original = graph(vec![
            object(vec![property(20, "b", 3), property(10, "a", 1)]),
            array(2), scalar(ValueKind::String), scalar(ValueKind::Boolean),
        ]);
        let mut changed = graph(vec![
            TypeNode::Ref(String::from("root")),
            scalar(ValueKind::Boolean),
            TypeNode::Ref(String::from("list")),
            scalar(ValueKind::String),
            object(vec![property(10, "a", 2), property(20, "b", 1)]),
            array(6),
            TypeNode::Ref(String::from("text")),
            text(Some("unused pattern"), Some("unknown")), array(8),
        ]);
        changed.definitions = vec![
            NamedType { name: String::from("text"), ty: TypeId(3) },
            NamedType { name: String::from("root"), ty: TypeId(4) },
            NamedType { name: String::from("list"), ty: TypeId(5) },
            NamedType { name: String::from("unused"), ty: TypeId(7) },
        ];
        let expected = canonical(original);
        assert_eq!(canonical(changed.clone()), expected);
        changed.root = TypeId(4);
        changed.definitions.reverse();
        for node in &mut changed.nodes {
            if let TypeNode::Ref(name) = node {
                name.push_str(" renamed\0é");
            }
        }
        for definition in &mut changed.definitions {
            definition.name.push_str(" renamed\0é");
        }
        assert_eq!(canonical(changed), expected);
    }

    #[test]
    fn property_sorting_is_exact_utf8_without_case_or_unicode_normalization() {
        let names = ["😀", "é", "a", "e\u{301}", "A", "\0", ""];
        let fields: Vec<Property> = names.iter().enumerate()
            .map(|(index, name)| property(index as u32, name, 1)).collect();
        let input = graph(vec![object(fields.clone()), TypeNode::Any]);
        let result = canonical(input);
        let order = [(6, ""), (5, "\0"), (4, "A"), (2, "a"), (3, "e\u{301}"),
            (1, "é"), (0, "😀")];
        let mut records = vec![2, 1, 0, 0, 7, 0, 0, 0];
        for (id, name) in order {
            records.extend_from_slice(&[id, 0, 0, 0]);
            records.extend_from_slice(&[name.len() as u8, 0, 0, 0]);
            records.extend_from_slice(name.as_bytes());
            records.extend_from_slice(&[1, 0, 0, 0, 0, 0]);
        }
        records.push(0);
        assert_eq!(result.bytes, expected(2, &records));
        for rotation in 0..fields.len() {
            let mut reordered = fields.clone();
            reordered.rotate_left(rotation);
            reordered.reverse();
            assert_eq!(canonical(graph(vec![object(reordered), TypeNode::Any])), result);
        }
    }

    #[test]
    fn self_recursive_and_mutually_recursive_graphs_are_finite() {
        assert_vector(graph(vec![array(0)]), 1, &[3, 0, 0, 0, 0, 0, 0]);
        assert_vector(graph(vec![object(vec![property(1, "self", 0)])]), 1, &[
            2, 1, 0, 0, 1, 0, 0, 0,
            1, 0, 0, 0, 4, 0, 0, 0, b's', b'e', b'l', b'f', 0, 0, 0, 0, 0, 0,
        ]);
        let mut input = graph(vec![
            object(vec![property(1, "x", 1)]), array(2), TypeNode::Ref(String::from("root")),
        ]);
        input.definitions.push(NamedType { name: String::from("root"), ty: TypeId(0) });
        assert_vector(input, 2, &[
            2, 1, 0, 0, 1, 0, 0, 0,
            1, 0, 0, 0, 1, 0, 0, 0, b'x', 1, 0, 0, 0, 0, 0,
            3, 0, 0, 0, 0, 0, 0,
        ]);
    }

    #[test]
    fn sharing_is_preserved_and_not_confused_with_recursion_or_deduplication() {
        let shared = resolved(graph(vec![
            object(vec![property(1, "a", 1), property(2, "b", 1)]), TypeNode::Any,
        ]));
        let separate = resolved(graph(vec![
            object(vec![property(1, "a", 1), property(2, "b", 2)]),
            TypeNode::Any, TypeNode::Any,
        ]));
        let policy = SchemaPolicy { allow_recursive: false, ..SchemaPolicy::default() };
        let left = canonicalize(&shared, &policy).unwrap();
        let right = canonicalize(&separate, &policy).unwrap();
        assert_eq!(&left.bytes[34..38], &[2, 0, 0, 0]);
        assert_eq!(&right.bytes[34..38], &[3, 0, 0, 0]);
        assert_ne!(left.bytes, right.bytes);
    }

    #[test]
    fn all_policy_fields_are_identity_inputs_with_explicit_tags_and_widths() {
        let schema = resolved(graph(vec![TypeNode::Any]));
        let policy = SchemaPolicy {
            dialect: Dialect::JsonSchema202012,
            numeric: NumericPolicy {
                allow_Quoted_int: true, reject_non_finite_float: false,
                reject_negative_zero: false, max_exponent_abs: i16::MAX,
            },
            unknown_formats: UnknownFormatPolicy::Annotate,
            allow_recursive: false, max_pattern_bytes: u32::MAX,
        };
        let result = canonicalize(&schema, &policy).unwrap();
        let mut bytes = expected(1, &[0]);
        bytes[14..26].copy_from_slice(&[
            1, 1, 0, 0, 0xff, 0x7f, 1, 0, 0xff, 0xff, 0xff, 0xff,
        ]);
        assert_eq!(result.bytes, bytes);
        let default = SchemaPolicy::default();
        let baseline = canonicalize(&schema, &default).unwrap();
        for field in 0..8 {
            let mut changed = default;
            match field {
                0 => changed.dialect = policy.dialect,
                1 => changed.numeric.allow_Quoted_int = true,
                2 => changed.numeric.reject_non_finite_float = false,
                3 => changed.numeric.reject_negative_zero = false,
                4 => changed.numeric.max_exponent_abs = 0,
                5 => changed.unknown_formats = policy.unknown_formats,
                6 => changed.allow_recursive = false,
                _ => changed.max_pattern_bytes = 1,
            }
            assert_ne!(canonicalize(&schema, &changed).unwrap().bytes, baseline.bytes);
        }
    }

    #[test]
    fn schema_metadata_and_property_metadata_remain_identity_inputs() {
        let mut input = graph(vec![TypeNode::Any]);
        input.name = String::from("é\0");
        input.schema_id = SchemaId(u32::MAX);
        let mut bytes = expected(1, &[0]);
        drop(bytes.splice(26..34, [3, 0, 0, 0, 0xc3, 0xa9, 0, 0xff, 0xff, 0xff, 0xff]));
        assert_eq!(canonical(input).bytes, bytes);

        let baseline = graph(vec![object(vec![property(1, "a", 1)]), TypeNode::Any]);
        let expected = canonical(baseline.clone());
        for change in 0..8 {
            let mut input = baseline.clone();
            if change == 0 {
                input.name.push('x');
            } else if change == 1 {
                input.schema_id = SchemaId(1);
            } else if let TypeNode::Object(object) = &mut input.nodes[0] {
                match change {
                    2 => object.properties[0].field_id = FieldId(2),
                    3 => object.properties[0].name.push('x'),
                    4 => object.properties[0].required = true,
                    5 => object.properties[0].slot = Some(0),
                    6 => object.reject_unknown = false,
                    _ => object.min_properties = Some(0),
                }
            }
            assert_ne!(canonical(input).bytes, expected.bytes);
        }
    }

    #[test]
    fn equivalent_bounds_and_annotations_keep_their_explicit_representations() {
        let first = canonical(graph(vec![decimal(Some(Decimal128::new(10, 1)), None)]));
        let second = canonical(graph(vec![decimal(Some(Decimal128::new(100, 2)), None)]));
        assert_ne!(first.bytes, second.bytes);
        let absent = canonical(graph(vec![array(1), TypeNode::Any]));
        let present = canonical(graph(vec![TypeNode::Array(ArrayShape {
            items: TypeId(1), min_items: Some(0), max_items: None,
        }), TypeNode::Any]));
        assert_ne!(absent.bytes, present.bytes);
        let policy = SchemaPolicy { unknown_formats: UnknownFormatPolicy::Annotate,
            ..SchemaPolicy::default() };
        let schemas: Vec<CanonicalSchema> = [None, Some(""), Some("custom"), Some("Custom")]
            .into_iter().map(|name| {
                canonicalize(&resolved(graph(vec![text(None, name)])), &policy).unwrap()
            }).collect();
        for (index, left) in schemas.iter().enumerate() {
            for right in &schemas[index + 1..] {
                assert_ne!(left.bytes, right.bytes);
            }
        }
        let mut bytes = expected(1, &[1, 5, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0]);
        bytes[20] = 1;
        assert_eq!(schemas[1].bytes, bytes);
    }

    #[test]
    fn normalize_resolve_policy_and_canonicalization_integrate_without_losing_slots() {
        let ir = SchemaIr {
            name: String::from("adapter"), schema_id: SchemaId(416),
            required_count: 1, reject_unknown: true,
            fields: vec![
                SchemaFieldIr {
                    field_id: FieldId(20), name: String::from("z"), required: true,
                    scalar_kind: Some(ScalarKind::Number), format: None,
                    min_len: None, max_len: None, min_i64: Some(-7), max_i64: Some(42),
                },
                SchemaFieldIr {
                    field_id: FieldId(10), name: String::from("a"), required: false,
                    scalar_kind: Some(ScalarKind::String), format: Some(String::from("hex")),
                    min_len: Some(0), max_len: Some(32), min_i64: None, max_i64: None,
                },
            ],
        };
        let normalized = model::normalize(&ir).unwrap();
        let schema = resolved(normalized.clone());
        let a = model::property(&schema, TypeId(0), "a").unwrap().unwrap();
        assert_eq!(a.slot, Some(1));
        assert_eq!(model::required_fields(&schema, TypeId(0)).unwrap(), vec![FieldId(20)]);
        assert_eq!(model::constraints(&schema, a.ty).unwrap().unwrap().max_length, Some(32));
        let baseline = canonicalize(&schema, &SchemaPolicy::default()).unwrap();
        let mut reordered = normalized;
        if let TypeNode::Object(object) = &mut reordered.nodes[0] {
            object.properties.reverse();
        }
        assert_eq!(canonical(reordered), baseline);
        let mut reordered_ir = ir;
        reordered_ir.fields.reverse();
        // The adapter assigns explicit slots from declaration order; changing
        // those slots intentionally changes identity even after name sorting.
        assert_ne!(canonical(model::normalize(&reordered_ir).unwrap()).bytes, baseline.bytes);
    }

    #[test]
    fn policy_errors_propagate_unchanged_through_both_public_entry_points() {
        let mut input = graph(vec![
            object(vec![property(42, "value", 1)]), text(Some("é"), Some("unknown")),
        ]);
        input.schema_id = SchemaId(416);
        let schema = resolved(input);
        let mut invalid_numeric = SchemaPolicy::default();
        invalid_numeric.numeric.max_exponent_abs = -1;
        for (policy, code, field) in [
            (invalid_numeric, ErrorCode::ConstraintViolation, 0),
            (SchemaPolicy { max_pattern_bytes: 0, ..SchemaPolicy::default() },
                ErrorCode::ConstraintViolation, 0),
            (SchemaPolicy { max_pattern_bytes: 1, ..SchemaPolicy::default() },
                ErrorCode::BudgetExhausted, 42),
            (SchemaPolicy::default(), ErrorCode::UnsupportedSchemaFeature, 42),
        ] {
            let original = policy::validate_schema(&schema, &policy).unwrap_err();
            assert_error(original.clone(), code, 416, field);
            assert_eq!(canonicalize(&schema, &policy).unwrap_err(), original);
            assert_eq!(fingerprint(&schema, &policy).unwrap_err(), original);
        }
        let schema = resolved(graph(vec![array(0)]));
        let policy = SchemaPolicy { allow_recursive: false, ..SchemaPolicy::default() };
        let original = policy::validate_schema(&schema, &policy).unwrap_err();
        assert_error(original.clone(), ErrorCode::UnsupportedSchemaFeature, 0, 0);
        assert_eq!(canonicalize(&schema, &policy).unwrap_err(), original);
        assert_eq!(fingerprint(&schema, &policy).unwrap_err(), original);
    }

    #[test]
    fn pattern_boundary_is_inclusive_and_unreachable_policy_failures_are_excluded() {
        let schema = resolved(graph(vec![text(Some("é"), Some("hex"))]));
        let policy = SchemaPolicy { max_pattern_bytes: 2, ..SchemaPolicy::default() };
        canonicalize(&schema, &policy).unwrap();
        let limited = SchemaPolicy { max_pattern_bytes: 1, ..policy };
        assert_error(canonicalize(&schema, &limited).unwrap_err(), ErrorCode::BudgetExhausted, 0, 0);
        let schema = resolved(graph(vec![
            TypeNode::Any, text(Some("too long"), Some("unknown")), array(2),
        ]));
        let policy = SchemaPolicy { allow_recursive: false, ..limited };
        let baseline = resolved(graph(vec![TypeNode::Any]));
        assert_eq!(canonicalize(&schema, &policy).unwrap(), canonicalize(&baseline, &policy).unwrap());
    }

    #[test]
    fn deep_chains_and_wide_shared_graphs_do_not_expand_paths() {
        let depth = 8192_u32;
        let mut nodes: Vec<TypeNode> = (0..depth).map(|index| array(index + 1)).collect();
        nodes.push(TypeNode::Any);
        let result = canonical(graph(nodes));
        assert_eq!(result.bytes.len(), 38 + depth as usize * 7 + 1);
        assert_eq!(&result.bytes[34..38], &(depth + 1).to_le_bytes());
        for index in 0..depth {
            let start = 38 + index as usize * 7;
            assert_eq!(result.bytes[start], 3);
            assert_eq!(&result.bytes[start + 1..start + 5], &(index + 1).to_le_bytes());
            assert_eq!(&result.bytes[start + 5..start + 7], &[0, 0]);
        }
        assert_eq!(result.bytes.last(), Some(&0));
        let fields = (0..1024).rev()
            .map(|index| property(index, &format!("field{index:04}"), 1)).collect();
        let result = canonical(graph(vec![object(fields), TypeNode::Any]));
        assert_eq!(&result.bytes[34..38], &[2, 0, 0, 0]);
        assert_eq!(result.bytes.len(), 38 + 8 + 1024 * (4 + 4 + 9 + 4 + 1 + 1) + 1);
    }

    #[test]
    fn invalid_discovery_is_an_error_and_does_not_change_traversal_state() {
        let mut input = graph(vec![TypeNode::Any]);
        input.schema_id = SchemaId(416);
        let schema = resolved(input);
        for ty in [TypeId(1), TypeId(u32::MAX)] {
            let mut indices = vec![UNDISCOVERED];
            let mut queue = Vec::new();
            let failure = discover(&schema, ty, Some(FieldId(42)), &mut indices, &mut queue)
                .unwrap_err();
            assert_error(failure, ErrorCode::ConstraintViolation, 416, 42);
            assert_eq!(indices, vec![UNDISCOVERED]);
            assert!(queue.is_empty());
        }
        let mut indices = Vec::new();
        let mut queue = Vec::new();
        assert_error(discover(&schema, TypeId(0), None, &mut indices, &mut queue).unwrap_err(),
            ErrorCode::ConstraintViolation, 416, 0);
        assert!(queue.is_empty());
    }

    #[test]
    fn count_and_capacity_overflow_report_budget_errors_without_large_allocations() {
        let id = SchemaId(416);
        assert_eq!(count(0, id, None).unwrap(), 0);
        if let Ok(maximum) = usize::try_from(u32::MAX) {
            assert_eq!(count(maximum, id, None).unwrap(), u32::MAX);
            if let Some(overflow) = maximum.checked_add(1) {
                assert_error(count(overflow, id, Some(FieldId(42))).unwrap_err(),
                    ErrorCode::BudgetExhausted, 416, 42);
            }
        }
        let mut bytes: Vec<u8> = Vec::new();
        assert_error(reserve(&mut bytes, usize::MAX, id).unwrap_err(),
            ErrorCode::BudgetExhausted, 416, 0);
        assert!(bytes.is_empty());
        assert_eq!(bytes.capacity(), 0);
    }
}
