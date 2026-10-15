//! Dialect capabilities and policy checks over resolved, reachable schemas.
//!
//! Dialects record provenance; neither dialect enables unmodeled keywords.
//! Importers must query each encountered feature before discarding its syntax.

use super::*;

/// Report a feature's support independently of whether the policy is valid.
/// Both dialects expose the same modeled features. This does not imply support
/// for arbitrary documents from either dialect or for remote resolution.
pub fn supports(policy: SchemaPolicy, feature: SchemaFeature) -> bool {
    match feature {
        SchemaFeature::LocalReferences | SchemaFeature::Patterns
        | SchemaFeature::Formats | SchemaFeature::Decimal => true,
        SchemaFeature::RecursiveTypes => policy.allow_recursive,
        SchemaFeature::Union | SchemaFeature::RemoteReferences
        | SchemaFeature::Conditional | SchemaFeature::UnevaluatedProperties => false,
    }
}

/// Check policy parameters without imposing additional restrictions on numeric
/// conversion flags. An exponent limit of zero is valid; a pattern budget of
/// zero is invalid even when the schema contains no patterns.
pub fn validate_policy(policy: &SchemaPolicy) -> Result<()> {
    if policy.numeric.max_exponent_abs < 0 {
        return Err(schema_error(ErrorCode::ConstraintViolation,
            "numeric exponent limit must be nonnegative"));
    }
    if policy.max_pattern_bytes == 0 {
        return Err(schema_error(ErrorCode::ConstraintViolation,
            "pattern byte limit must be nonzero"));
    }
    Ok(())
}

/// Recognize exactly the case-sensitive names in format registry version 1.
/// No trimming, case folding, aliases, or Unicode normalization is performed.
pub fn known_format(name: &str) -> bool {
    matches!(name,
        "uuid" | "ipv4" | "ipv6" | "ip" | "base64" | "base64url" | "hex"
        | "nanoid" | "blockchain-address" | "blockchain-transaction-hash")
}

fn error(schema: &ResolvedSchema, field: Option<FieldId>, code: ErrorCode,
    detail: &'static str) -> Error
{
    schema_error(code, detail)
        .with_schema(schema.schema().schema_id.0, field.map_or(0, |id| id.0))
}

fn reserve<T>(values: &mut Vec<T>, count: usize, schema: &ResolvedSchema) -> Result<()> {
    values.try_reserve_exact(count).map_err(|_| {
        error(schema, None, ErrorCode::BudgetExhausted, "policy traversal allocation failed")
    })
}

// Mark at discovery, not at processing, so shared targets and back edges never
// grow the queue beyond the stored node count reserved by validate_schema.
fn enqueue(schema: &ResolvedSchema, ty: TypeId, field: Option<FieldId>,
    seen: &mut [bool], queue: &mut Vec<(TypeId, Option<FieldId>)>) -> Result<()>
{
    let target = resolve::resolve_type(schema, ty).map_err(|failure| {
        let field_id = field.map_or(failure.field_id, |id| id.0);
        failure.with_schema(schema.schema().schema_id.0, field_id)
    })?;
    if !seen[target.0 as usize] {
        seen[target.0 as usize] = true;
        queue.push((target, field));
    }
    Ok(())
}

fn validate_scalar_policy(schema: &ResolvedSchema, value: &ScalarConstraints,
    policy: &SchemaPolicy, field: Option<FieldId>) -> Result<()>
{
    if let Some(pattern) = &value.pattern {
        if u32::try_from(pattern.len()).map_or(true, |len| len > policy.max_pattern_bytes) {
            return Err(error(schema, field, ErrorCode::BudgetExhausted,
                "pattern UTF-8 byte limit exceeded"));
        }
    }
    if let Some(format) = &value.format {
        if policy.unknown_formats == UnknownFormatPolicy::Reject && !known_format(format) {
            return Err(error(schema, field, ErrorCode::UnsupportedSchemaFeature,
                "unknown format name"));
        }
    }
    Ok(())
}

/// Validate policy and all root-reachable constraints after alias collapse.
///
/// The resolver's immutable witness already guarantees structural validity,
/// compatible constraint families, and valid numeric and container bounds.
/// This pass adds pattern byte budgets, format support, and recursion policy.
/// Regex syntax belongs to the compiler. Unknown formats under Annotate are
/// retained verbatim for canonicalization; lowering must omit their validators.
///
/// Definitions alone do not make a node reachable. The iterative walk checks
/// each reachable target once in breadth-first, stored property order, carrying
/// the first discovered field context through aliases and array items. All
/// traversal storage is fallibly reserved and bounded by the stored node count.
/// Neither the schema nor the policy is modified.
pub fn validate_schema(schema: &ResolvedSchema, policy: &SchemaPolicy) -> Result<()> {
    validate_policy(policy).map_err(|failure| {
        failure.with_schema(schema.schema().schema_id.0, 0)
    })?;
    if !policy.allow_recursive && resolve::has_recursion(schema)? {
        return Err(error(schema, None, ErrorCode::UnsupportedSchemaFeature,
            "recursive types are disabled by policy"));
    }

    let count = schema.schema().nodes.len();
    let mut seen = Vec::new();
    let mut queue = Vec::new();
    reserve(&mut seen, count, schema)?;
    reserve(&mut queue, count, schema)?;
    seen.resize(count, false);
    enqueue(schema, schema.schema().root, None, &mut seen, &mut queue)?;

    let mut next = 0;
    while next < queue.len() {
        let (ty, field) = queue[next];
        next += 1;
        // enqueue checked the collapsed target and its node bounds.
        match &schema.schema().nodes[ty.0 as usize] {
            TypeNode::Any => {}
            TypeNode::Scalar(value) => validate_scalar_policy(schema, value, policy, field)?,
            TypeNode::Object(object) => {
                for property in &object.properties {
                    enqueue(schema, property.ty, Some(property.field_id), &mut seen, &mut queue)?;
                }
            }
            TypeNode::Array(array) => {
                enqueue(schema, array.items, field, &mut seen, &mut queue)?;
            }
            TypeNode::Ref(_) => {
                return Err(error(schema, field, ErrorCode::ConstraintViolation,
                    "resolved target must not be a reference"));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{format, vec};

    const ID: SchemaId = SchemaId(316);
    const FORMATS: [&str; 10] = [
        "uuid", "ipv4", "ipv6", "ip", "base64", "base64url", "hex", "nanoid",
        "blockchain-address", "blockchain-transaction-hash",
    ];

    fn graph(nodes: Vec<TypeNode>) -> Schema {
        Schema {
            name: String::from("policy tests"), schema_id: ID, root: TypeId(0),
            nodes, definitions: Vec::new(),
        }
    }

    fn resolved(input: Schema) -> ResolvedSchema {
        resolve::resolve(input, ResolveLimits::default()).unwrap()
    }

    fn string(pattern: Option<&str>, format: Option<&str>) -> TypeNode {
        let mut value = ScalarConstraints::new(ValueKind::String);
        value.pattern = pattern.map(String::from);
        value.format = format.map(String::from);
        TypeNode::Scalar(value)
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

    fn definition(name: &str, ty: u32) -> NamedType {
        NamedType { name: String::from(name), ty: TypeId(ty) }
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
    fn feature_matrix_is_identical_for_both_dialects() {
        for dialect in [Dialect::Endpoint, Dialect::JsonSchema202012] {
            for allow_recursive in [false, true] {
                let policy = SchemaPolicy { dialect, allow_recursive, ..SchemaPolicy::default() };
                for feature in [SchemaFeature::LocalReferences, SchemaFeature::Patterns,
                    SchemaFeature::Formats, SchemaFeature::Decimal]
                {
                    assert!(supports(policy, feature));
                }
                assert_eq!(supports(policy, SchemaFeature::RecursiveTypes), allow_recursive);
                for feature in [SchemaFeature::Union, SchemaFeature::RemoteReferences,
                    SchemaFeature::Conditional, SchemaFeature::UnevaluatedProperties]
                {
                    assert!(!supports(policy, feature));
                }
            }
        }
    }

    #[test]
    fn support_queries_do_not_silently_validate_or_repair_policy() {
        let mut policy = SchemaPolicy { max_pattern_bytes: 0, ..SchemaPolicy::default() };
        policy.numeric.max_exponent_abs = -1;
        let before = policy;
        assert!(supports(policy, SchemaFeature::Patterns));
        assert!(!supports(policy, SchemaFeature::RemoteReferences));
        assert!(validate_policy(&policy).is_err());
        assert_eq!(policy, before);
    }

    #[test]
    fn policy_boundaries_and_numeric_flag_combinations_are_valid() {
        for dialect in [Dialect::Endpoint, Dialect::JsonSchema202012] {
            for max_exponent_abs in [0, 1, 308, i16::MAX] {
                for max_pattern_bytes in [1, 4096, u32::MAX] {
                    for flags in 0..8 {
                        let policy = SchemaPolicy {
                            dialect, max_pattern_bytes,
                            numeric: NumericPolicy {
                                allow_Quoted_int: flags & 1 != 0,
                                reject_non_finite_float: flags & 2 != 0,
                                reject_negative_zero: flags & 4 != 0,
                                max_exponent_abs,
                            },
                            ..SchemaPolicy::default()
                        };
                        validate_policy(&policy).unwrap();
                    }
                }
            }
        }
        validate_policy(&SchemaPolicy::default()).unwrap();
    }

    #[test]
    fn invalid_policy_errors_have_compile_source_and_schema_context_when_available() {
        let schema = resolved(graph(vec![TypeNode::Any]));
        let mut policies = vec![SchemaPolicy { max_pattern_bytes: 0, ..SchemaPolicy::default() }];
        for maximum in [i16::MIN, -1] {
            let mut policy = SchemaPolicy::default();
            policy.numeric.max_exponent_abs = maximum;
            policies.push(policy);
        }
        for policy in policies {
            assert_error(validate_policy(&policy).unwrap_err(), ErrorCode::ConstraintViolation, 0, 0);
            assert_error(validate_schema(&schema, &policy).unwrap_err(),
                ErrorCode::ConstraintViolation, ID.0, 0);
        }
    }

    #[test]
    fn invalid_policy_precedes_recursion_and_scalar_policy_failures() {
        let schema = resolved(graph(vec![
            object(vec![property(1, "self", 0), property(2, "value", 1)]),
            string(Some("xx"), Some("unknown")),
        ]));
        let policy = SchemaPolicy {
            allow_recursive: false, max_pattern_bytes: 0, ..SchemaPolicy::default()
        };
        assert_error(validate_schema(&schema, &policy).unwrap_err(),
            ErrorCode::ConstraintViolation, ID.0, 0);
    }

    #[test]
    fn registry_names_are_exact_and_case_sensitive() {
        for name in FORMATS {
            assert!(known_format(name));
            for altered in [name.to_uppercase(), format!(" {name}"), format!("{name} "),
                format!("{name}\0"), format!("{name}-extra")]
            {
                assert!(!known_format(&altered));
            }
        }
        for name in ["", "UUID", "Uuid", "date-time", "email", "uri", "hostname", "regex",
            "int64", "base64-url", "blockchain_transaction_hash", "ｕｕｉｄ", "é", "\0"]
        {
            assert!(!known_format(name));
        }
    }

    #[test]
    fn all_registry_formats_validate_in_both_dialects_and_format_modes() {
        for name in FORMATS {
            let schema = resolved(graph(vec![string(None, Some(name))]));
            for dialect in [Dialect::Endpoint, Dialect::JsonSchema202012] {
                for unknown_formats in [UnknownFormatPolicy::Reject, UnknownFormatPolicy::Annotate] {
                    let policy = SchemaPolicy { dialect, unknown_formats, ..SchemaPolicy::default() };
                    validate_schema(&schema, &policy).unwrap();
                }
            }
        }
    }

    #[test]
    fn unknown_formats_reject_or_remain_exact_annotations() {
        for name in ["", "UUID", "date-time", "unknown", " custom\0é "] {
            let schema = resolved(graph(vec![string(None, Some(name))]));
            let before = schema.schema().clone();
            for dialect in [Dialect::Endpoint, Dialect::JsonSchema202012] {
                let reject = SchemaPolicy { dialect, ..SchemaPolicy::default() };
                assert_error(validate_schema(&schema, &reject).unwrap_err(),
                    ErrorCode::UnsupportedSchemaFeature, ID.0, 0);
                let annotate = SchemaPolicy { unknown_formats: UnknownFormatPolicy::Annotate, ..reject };
                validate_schema(&schema, &annotate).unwrap();
                assert_eq!(schema.schema(), &before);
                assert_eq!(model::constraints(&schema, TypeId(0)).unwrap().unwrap().format.as_deref(),
                    Some(name));
            }
        }
        validate_schema(&resolved(graph(vec![string(None, None)])), &SchemaPolicy::default()).unwrap();
    }

    #[test]
    fn pattern_limits_count_utf8_bytes_and_include_the_boundary() {
        for (pattern, limit, allowed) in [
            ("", 1, true), ("a", 1, true), ("ab", 1, false),
            ("é", 1, false), ("é", 2, true), ("éa", 2, false),
            ("😀", 3, false), ("😀", 4, true),
            ("e\u{301}", 2, false), ("e\u{301}", 3, true),
            ("\0", 1, true), ("x", u32::MAX, true),
        ] {
            let schema = resolved(graph(vec![string(Some(pattern), None)]));
            let policy = SchemaPolicy { max_pattern_bytes: limit, ..SchemaPolicy::default() };
            if allowed {
                validate_schema(&schema, &policy).unwrap();
            } else {
                assert_error(validate_schema(&schema, &policy).unwrap_err(),
                    ErrorCode::BudgetExhausted, ID.0, 0);
            }
        }
        for length in [4095, 4096, 4097] {
            let pattern = "x".repeat(length);
            let schema = resolved(graph(vec![string(Some(&pattern), None)]));
            let result = validate_schema(&schema, &SchemaPolicy::default());
            if length <= 4096 {
                result.unwrap();
            } else {
                assert_error(result.unwrap_err(), ErrorCode::BudgetExhausted, ID.0, 0);
            }
        }
    }

    #[test]
    fn pattern_budgets_are_per_pattern_and_syntax_is_left_to_compilation() {
        let schema = resolved(graph(vec![
            object(vec![property(1, "first", 1), property(2, "second", 2)]),
            string(Some("["), Some("hex")), string(Some("("), None),
        ]));
        let policy = SchemaPolicy { max_pattern_bytes: 1, ..SchemaPolicy::default() };
        validate_schema(&schema, &policy).unwrap();
    }

    #[test]
    fn annotation_mode_does_not_bypass_pattern_limits() {
        let schema = resolved(graph(vec![
            object(vec![property(42, "value", 1)]), string(Some("é"), Some("unknown")),
        ]));
        let policy = SchemaPolicy {
            unknown_formats: UnknownFormatPolicy::Annotate, max_pattern_bytes: 1,
            ..SchemaPolicy::default()
        };
        assert_error(validate_schema(&schema, &policy).unwrap_err(), ErrorCode::BudgetExhausted, ID.0, 42);
    }

    #[test]
    fn unreachable_patterns_formats_and_recursion_do_not_affect_policy() {
        let mut input = graph(vec![
            TypeNode::Any, string(Some("too long"), Some("unknown")), array(2),
            TypeNode::Ref(String::from("unused")),
        ]);
        input.definitions = vec![definition("unused", 1), definition("cycle", 2)];
        let schema = resolved(input);
        let policy = SchemaPolicy {
            allow_recursive: false, max_pattern_bytes: 1, ..SchemaPolicy::default()
        };
        validate_schema(&schema, &policy).unwrap();
    }

    #[test]
    fn aliases_and_array_items_preserve_field_context() {
        for (leaf, code) in [
            (string(None, Some("unknown")), ErrorCode::UnsupportedSchemaFeature),
            (string(Some("é"), None), ErrorCode::BudgetExhausted),
        ] {
            let mut input = graph(vec![
                object(vec![property(u32::MAX, "", 1)]),
                TypeNode::Ref(String::from("list")), array(3),
                TypeNode::Ref(String::from("alias")), TypeNode::Ref(String::from("leaf")), leaf,
            ]);
            input.definitions = vec![definition("list", 2), definition("alias", 4), definition("leaf", 5)];
            let schema = resolved(input);
            let before = schema.schema().clone();
            let policy = SchemaPolicy { max_pattern_bytes: 1, ..SchemaPolicy::default() };
            assert_error(validate_schema(&schema, &policy).unwrap_err(), code, ID.0, u32::MAX);
            assert_eq!(schema.schema(), &before);
        }
    }

    #[test]
    fn alias_root_at_nonzero_index_checks_its_target() {
        let mut input = graph(vec![string(None, Some("unknown")), TypeNode::Ref(String::from("root"))]);
        input.root = TypeId(1);
        input.definitions.push(definition("root", 0));
        let schema = resolved(input);
        assert_error(validate_schema(&schema, &SchemaPolicy::default()).unwrap_err(),
            ErrorCode::UnsupportedSchemaFeature, ID.0, 0);
    }

    #[test]
    fn nested_properties_replace_outer_field_context() {
        let schema = resolved(graph(vec![
            object(vec![property(10, "outer", 1)]), array(2),
            object(vec![property(20, "inner", 3)]), string(None, Some("unknown")),
        ]));
        assert_error(validate_schema(&schema, &SchemaPolicy::default()).unwrap_err(),
            ErrorCode::UnsupportedSchemaFeature, ID.0, 20);
    }

    #[test]
    fn shared_targets_use_first_discovered_field_context_deterministically() {
        let schema = resolved(graph(vec![
            object(vec![property(42, "first", 1), property(43, "second", 1)]),
            string(None, Some("unknown")),
        ]));
        for _ in 0..3 {
            assert_error(validate_schema(&schema, &SchemaPolicy::default()).unwrap_err(),
                ErrorCode::UnsupportedSchemaFeature, ID.0, 42);
        }
    }

    #[test]
    fn recursive_objects_arrays_and_alias_cycles_follow_policy() {
        let mut mutual = graph(vec![
            TypeNode::Ref(String::from("object")), object(vec![property(42, "items", 2)]),
            array(0),
        ]);
        mutual.definitions.push(definition("object", 1));
        for input in [graph(vec![array(0)]), graph(vec![object(vec![property(42, "self", 0)])]), mutual] {
            let schema = resolved(input);
            let before = schema.schema().clone();
            for dialect in [Dialect::Endpoint, Dialect::JsonSchema202012] {
                let policy = SchemaPolicy { dialect, ..SchemaPolicy::default() };
                validate_schema(&schema, &policy).unwrap();
                let disabled = SchemaPolicy { allow_recursive: false, ..policy };
                assert_error(validate_schema(&schema, &disabled).unwrap_err(),
                    ErrorCode::UnsupportedSchemaFeature, ID.0, 0);
            }
            assert_eq!(schema.schema(), &before);
        }
    }

    #[test]
    fn recursive_walk_still_checks_later_properties() {
        let schema = resolved(graph(vec![
            object(vec![property(1, "self", 0), property(42, "later", 1)]),
            string(None, Some("unknown")),
        ]));
        assert_error(validate_schema(&schema, &SchemaPolicy::default()).unwrap_err(),
            ErrorCode::UnsupportedSchemaFeature, ID.0, 42);
    }

    #[test]
    fn shared_diamonds_and_many_duplicate_edges_are_not_recursion() {
        let mut properties = vec![property(1, "left", 1), property(2, "right", 2)];
        for index in 3..1027 {
            properties.push(property(index, &format!("shared{index}"), 3));
        }
        let schema = resolved(graph(vec![
            object(properties), array(3), object(vec![property(2048, "leaf", 3)]),
            string(Some("x"), Some("uuid")),
        ]));
        let policy = SchemaPolicy { allow_recursive: false, ..SchemaPolicy::default() };
        validate_schema(&schema, &policy).unwrap();
    }

    #[test]
    fn all_modeled_value_kinds_and_extreme_valid_bounds_are_supported() {
        let mut integer = ScalarConstraints::new(ValueKind::Integer);
        integer.min_i64 = Some(i64::MIN);
        integer.max_i64 = Some(i64::MAX);
        let mut decimal = ScalarConstraints::new(ValueKind::Decimal);
        decimal.decimal = Some(DecimalConstraints {
            policy: DecimalPolicy {
                max_precision: 38, target_scale: 38, allow_exponent: true,
                reject_inexact_rescale: true, tick_step: 0,
            },
            minimum: Some(Decimal128::new(i128::MIN, 38)),
            maximum: Some(Decimal128::new(i128::MAX, 38)),
        });
        let mut text = ScalarConstraints::new(ValueKind::String);
        text.min_length = Some(0);
        text.max_length = Some(u32::MAX);
        let nodes = vec![
            object((1..=8).map(|index| property(index, &format!("v{index}"), index)).collect()),
            TypeNode::Any, TypeNode::Scalar(ScalarConstraints::new(ValueKind::Null)),
            TypeNode::Scalar(ScalarConstraints::new(ValueKind::Boolean)),
            TypeNode::Scalar(integer), TypeNode::Scalar(ScalarConstraints::new(ValueKind::Number)),
            TypeNode::Scalar(decimal), TypeNode::Scalar(text),
            TypeNode::Array(ArrayShape { items: TypeId(1), min_items: Some(0), max_items: Some(u32::MAX) }),
        ];
        let schema = resolved(graph(nodes));
        let before = schema.schema().clone();
        for dialect in [Dialect::Endpoint, Dialect::JsonSchema202012] {
            let policy = SchemaPolicy { dialect, allow_recursive: false, ..SchemaPolicy::default() };
            validate_schema(&schema, &policy).unwrap();
            assert_eq!(schema.schema(), &before);
        }
        for object in [object(Vec::new()), TypeNode::Object(ObjectShape {
            properties: Vec::new(), reject_unknown: false,
            min_properties: Some(0), max_properties: Some(u32::MAX),
        })] {
            validate_schema(&resolved(graph(vec![object])), &SchemaPolicy::default()).unwrap();
        }
    }

    #[test]
    fn invalid_type_discovery_returns_context_without_modifying_traversal() {
        let schema = resolved(graph(vec![TypeNode::Any]));
        for ty in [TypeId(1), TypeId(u32::MAX)] {
            let mut seen = vec![false];
            let mut queue = Vec::new();
            assert_error(enqueue(&schema, ty, Some(FieldId(42)), &mut seen, &mut queue).unwrap_err(),
                ErrorCode::ConstraintViolation, ID.0, 42);
            assert_eq!(seen, vec![false]);
            assert!(queue.is_empty());
        }
    }

    #[test]
    fn deep_array_chains_are_stack_independent() {
        let depth = 8192_u32;
        let mut nodes = Vec::new();
        for index in 0..depth {
            nodes.push(array(index + 1));
        }
        nodes.push(string(Some("x"), Some("hex")));
        let schema = resolved(graph(nodes));
        for allow_recursive in [false, true] {
            let policy = SchemaPolicy { allow_recursive, max_pattern_bytes: 1, ..SchemaPolicy::default() };
            validate_schema(&schema, &policy).unwrap();
        }
    }

    #[test]
    fn capacity_overflow_is_a_budget_error_without_allocating() {
        let schema = resolved(graph(vec![TypeNode::Any]));
        let mut values: Vec<u8> = Vec::new();
        assert_error(reserve(&mut values, usize::MAX, &schema).unwrap_err(),
            ErrorCode::BudgetExhausted, ID.0, 0);
        assert!(values.is_empty());
        assert_eq!(values.capacity(), 0);
    }
}
