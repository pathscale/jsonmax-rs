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
        | "nanoid" | "blockchain-address" | "blockchain-transaction-hash"
        | "nanoid-chars" | "base62")
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
            TypeNode::Enum(_) => {}
            TypeNode::Ref(_) => {
                return Err(error(schema, field, ErrorCode::ConstraintViolation,
                    "resolved target must not be a reference"));
            }
        }
    }
    Ok(())
}
