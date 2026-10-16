//! Native decoder specification and its lowering to a nested schema graph.
//!
//! The specification is a separate artifact: it never changes what a model
//! means, only which structs get a native decoder and how they are spelled on
//! the wire. Lowering produces a tree of `jmax_schema_ir` nodes with no
//! pattern, reference or definition, so the reference program compiled from it
//! is exactly the document the native decoder promises to decide.

use super::{
    model_error, registry, EndpointField, EndpointType, Error, ErrorCode, FieldId, Registry,
    Result, SchemaId, MAX_REFERENCE_DEPTH,
};
use alloc::{string::String, vec::Vec};
use jmax_abi::number::DecimalPolicy;
use jmax_schema_ir::{
    ArrayShape, DecimalConstraints, ObjectShape, Property, ScalarConstraints, Schema, TypeId,
    TypeNode, ValueKind,
};

/// Rust field identifier chosen for one wire field of one nominal struct.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeIdent {
    pub type_name: String,
    pub wire: String,
    pub ident: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeSpec {
    /// Definition names that get a decoder entry point.
    pub roots: Vec<String>,
    /// Definition names of structs spelled as JSON arrays.
    pub tuples: Vec<String>,
    /// Applied to every BlockchainDecimal.
    pub decimal_policy: DecimalPolicy,
    /// Applied to every object.
    pub reject_unknown: bool,
    pub idents: Vec<NativeIdent>,
}

impl Default for NativeSpec {
    fn default() -> Self {
        Self {
            roots: Vec::new(),
            tuples: Vec::new(),
            decimal_policy: DecimalPolicy::spot_8(),
            reject_unknown: true,
            idents: Vec::new(),
        }
    }
}

const KEYWORDS: &[&str] = &[
    "as", "async", "await", "become", "box", "break", "const", "continue", "crate", "do", "dyn",
    "else", "enum", "extern", "false", "final", "fn", "for", "gen", "if", "impl", "in", "let",
    "loop", "macro", "match", "mod", "move", "mut", "override", "priv", "pub", "ref", "return",
    "self", "Self", "static", "struct", "super", "trait", "true", "try", "type", "typeof",
    "union", "unsafe", "unsized", "use", "virtual", "where", "while", "yield", "abstract",
];

fn unsupported(detail: &'static str) -> Error {
    model_error(ErrorCode::UnsupportedSchemaFeature, detail)
}

fn invalid(detail: &'static str) -> Error {
    model_error(ErrorCode::ConstraintViolation, detail)
}

/// The fields of a definition that must be a Struct.
fn struct_fields<'a>(registry: &Registry<'a>, name: &str) -> Result<&'a [EndpointField]> {
    let definition = registry::definition_by_name(registry, name)
        .ok_or_else(|| invalid("native specification names an unknown definition"))?;
    match registry::resolve_type(registry, &definition.ty)? {
        EndpointType::Struct { fields, .. } => Ok(fields),
        _ => Err(invalid("native specification name must be a struct definition")),
    }
}

fn has_duplicates(names: &[String]) -> bool {
    names.iter().enumerate().any(|(index, name)| names[..index].contains(name))
}

fn valid_ident(ident: &str) -> bool {
    let mut bytes = ident.bytes();
    matches!(bytes.next(), Some(b'a'..=b'z' | b'_'))
        && bytes.all(|byte| matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'_'))
        && ident != "_"
        && !KEYWORDS.contains(&ident)
}

/// Check the specification against the model it will decode.
pub fn validate_native_spec(registry: &Registry<'_>, spec: &NativeSpec) -> Result<()> {
    if has_duplicates(&spec.roots) || has_duplicates(&spec.tuples) {
        return Err(invalid("duplicate native root or tuple"));
    }
    for root in &spec.roots {
        if spec.tuples.contains(root) {
            return Err(invalid("a native root cannot also be a tuple"));
        }
        struct_fields(registry, root)?;
    }
    for tuple in &spec.tuples {
        if struct_fields(registry, tuple)?.is_empty() {
            return Err(invalid("a tuple needs at least one field"));
        }
    }
    let policy = &spec.decimal_policy;
    if !(1..=38).contains(&policy.max_precision) || policy.target_scale > 38 || policy.tick_step < 0 {
        return Err(invalid("invalid native decimal policy"));
    }
    for (index, ident) in spec.idents.iter().enumerate() {
        if !valid_ident(&ident.ident) {
            return Err(invalid("native ident is not a snake_case non-keyword identifier"));
        }
        let fields = struct_fields(registry, &ident.type_name)?;
        if !fields.iter().any(|field| field.name == ident.wire) {
            return Err(invalid("native ident names an unknown wire field"));
        }
        for earlier in &spec.idents[..index] {
            if earlier.type_name == ident.type_name
                && (earlier.wire == ident.wire || earlier.ident == ident.ident)
            {
                return Err(invalid("native ident repeats a field or identifier"));
            }
        }
    }
    Ok(())
}

struct Lowering<'r, 'a> {
    registry: &'r Registry<'a>,
    spec: &'r NativeSpec,
    nodes: Vec<TypeNode>,
    next_field: u32,
    /// Names of structs being lowered, for recursion detection.
    active: Vec<&'a str>,
}

/// Lower `root`, one of `spec.roots`, to a nested schema graph.
pub fn schema_graph_for(
    registry: &Registry<'_>,
    spec: &NativeSpec,
    root: &str,
    schema_id: SchemaId,
) -> Result<Schema> {
    validate_native_spec(registry, spec)?;
    if !spec.roots.iter().any(|candidate| candidate == root) {
        return Err(invalid("schema graph root is not listed in the native specification"));
    }
    let definition = registry::definition_by_name(registry, root)
        .ok_or_else(|| invalid("native root is not a definition"))?;
    let mut lowering = Lowering {
        registry,
        spec,
        nodes: Vec::new(),
        next_field: 1,
        active: Vec::new(),
    };
    let ty = lowering.lower(&definition.ty, 0).map_err(|error| error.with_schema(schema_id.0, 0))?;
    debug_assert_eq!(ty, TypeId(0));
    let schema = Schema {
        name: String::from(root),
        schema_id,
        root: ty,
        nodes: lowering.nodes,
        definitions: Vec::new(),
    };
    jmax_schema_ir::model::validate(&schema)?;
    Ok(schema)
}

impl<'r, 'a> Lowering<'r, 'a> {
    fn push(&mut self, node: TypeNode) -> Result<TypeId> {
        let id = u32::try_from(self.nodes.len())
            .map_err(|_| model_error(ErrorCode::BudgetExhausted, "schema node count exceeds u32"))?;
        self.nodes.push(node);
        Ok(TypeId(id))
    }

    fn scalar(&mut self, kind: ValueKind) -> Result<TypeId> {
        self.push(TypeNode::Scalar(ScalarConstraints::new(kind)))
    }

    fn integer(&mut self, min: i64, max: i64) -> Result<TypeId> {
        let mut constraints = ScalarConstraints::new(ValueKind::Integer);
        constraints.min_i64 = Some(min);
        constraints.max_i64 = Some(max);
        self.push(TypeNode::Scalar(constraints))
    }

    fn string(&mut self, format: Option<&'static str>) -> Result<TypeId> {
        let mut constraints = ScalarConstraints::new(ValueKind::String);
        if let Some(format) = format {
            jmax_format_lookup(format)?;
            constraints.format = Some(String::from(format));
        }
        self.push(TypeNode::Scalar(constraints))
    }

    fn lower(&mut self, ty: &'a EndpointType, depth: usize) -> Result<TypeId> {
        if depth > MAX_REFERENCE_DEPTH {
            return Err(model_error(ErrorCode::DepthLimitExceeded, "native schema nesting exceeds model limit"));
        }
        match ty {
            EndpointType::UInt32 => self.integer(0, i64::from(u32::MAX)),
            EndpointType::Int32 => self.integer(i64::from(i32::MIN), i64::from(i32::MAX)),
            EndpointType::Int64 | EndpointType::TimeStampMs => self.integer(i64::MIN, i64::MAX),
            EndpointType::Float64 => self.scalar(ValueKind::Number),
            EndpointType::Boolean => self.scalar(ValueKind::Boolean),
            EndpointType::String => self.string(None),
            EndpointType::UUID => self.string(Some("uuid")),
            EndpointType::IpAddr => self.string(Some("ip")),
            EndpointType::Bytea => self.string(Some("base64")),
            EndpointType::NanoId { len: 21 } => self.string(Some("nanoid")),
            EndpointType::BlockchainAddress => self.string(Some("blockchain-address")),
            EndpointType::BlockchainTransactionHash => {
                self.string(Some("blockchain-transaction-hash"))
            }
            EndpointType::BlockchainDecimal => {
                let mut constraints = ScalarConstraints::new(ValueKind::Decimal);
                constraints.decimal = Some(DecimalConstraints {
                    policy: self.spec.decimal_policy,
                    minimum: None,
                    maximum: None,
                });
                self.push(TypeNode::Scalar(constraints))
            }
            EndpointType::Vec(inner) => {
                if matches!(inner.as_ref(), EndpointType::Optional(_)) {
                    return Err(unsupported("optional array elements have no native form"));
                }
                let array = self.push(TypeNode::Array(ArrayShape {
                    items: TypeId(0),
                    min_items: None,
                    max_items: None,
                }))?;
                let items = self.lower(inner, depth + 1)?;
                if let TypeNode::Array(shape) = &mut self.nodes[array.0 as usize] {
                    shape.items = items;
                }
                Ok(array)
            }
            EndpointType::Struct { name, fields } => self.lower_struct(name, fields, depth),
            EndpointType::StructRef(_) | EndpointType::StructTable { .. } => {
                match registry::resolve_type(self.registry, ty)? {
                    EndpointType::Struct { name, fields } => self.lower_struct(name, fields, depth),
                    _ => Err(invalid("struct reference does not resolve to a struct")),
                }
            }
            EndpointType::Optional(_) => Err(unsupported("nested optional has no native form")),
            EndpointType::NanoId { .. } => Err(unsupported("only 21 byte NanoId has a native form")),
            EndpointType::Object => Err(unsupported("untyped Object has no native form")),
            EndpointType::Enum { .. } | EndpointType::EnumRef { .. } => {
                Err(unsupported("enums have no native form"))
            }
            EndpointType::Unit => Err(unsupported("unit has no native form")),
        }
    }

    fn lower_struct(
        &mut self,
        name: &'a str,
        fields: &'a [EndpointField],
        depth: usize,
    ) -> Result<TypeId> {
        if self.active.contains(&name) {
            return Err(unsupported("recursive types have no native form"));
        }
        self.active.push(name);
        let result = if self.spec.tuples.iter().any(|tuple| tuple == name) {
            self.lower_tuple(fields, depth)
        } else {
            self.lower_object(fields, depth)
        };
        self.active.pop();
        result
    }

    fn lower_object(&mut self, fields: &'a [EndpointField], depth: usize) -> Result<TypeId> {
        let object = self.push(TypeNode::Object(ObjectShape {
            properties: Vec::new(),
            reject_unknown: self.spec.reject_unknown,
            min_properties: None,
            max_properties: None,
        }))?;
        let mut properties = Vec::new();
        properties.try_reserve_exact(fields.len())
            .map_err(|_| model_error(ErrorCode::BudgetExhausted, "schema allocation failed"))?;
        for field in fields {
            if field.name.is_empty() {
                return Err(invalid("empty field name"));
            }
            let (ty, required) = match &field.ty {
                EndpointType::Optional(inner) => (inner.as_ref(), false),
                ty => (ty, true),
            };
            let field_id = FieldId(self.next_field);
            self.next_field = self.next_field.checked_add(1)
                .ok_or_else(|| model_error(ErrorCode::BudgetExhausted, "field id overflow"))?;
            let ty = self.lower(ty, depth + 1)?;
            properties.push(Property {
                field_id,
                name: field.name.clone(),
                ty,
                required,
                slot: None,
            });
        }
        if let TypeNode::Object(shape) = &mut self.nodes[object.0 as usize] {
            shape.properties = properties;
        }
        Ok(object)
    }

    fn lower_tuple(&mut self, fields: &'a [EndpointField], depth: usize) -> Result<TypeId> {
        let count = u32::try_from(fields.len())
            .map_err(|_| model_error(ErrorCode::BudgetExhausted, "tuple length exceeds u32"))?;
        if count == 0 {
            return Err(unsupported("empty tuple has no native form"));
        }
        let array = self.push(TypeNode::Array(ArrayShape {
            items: TypeId(0),
            min_items: Some(count),
            max_items: Some(count),
        }))?;
        let mut items = None;
        for field in fields {
            if matches!(field.ty, EndpointType::Optional(_)) {
                return Err(unsupported("optional tuple elements have no native form"));
            }
            let mark = self.nodes.len();
            let saved_field = self.next_field;
            let ty = self.lower(&field.ty, depth + 1)?;
            match items {
                None => items = Some(ty),
                Some(first) => {
                    if !self.same(first, ty) {
                        return Err(unsupported("heterogeneous tuples have no native form"));
                    }
                    // The element is a duplicate of the shared items node.
                    self.nodes.truncate(mark);
                    self.next_field = saved_field;
                }
            }
        }
        if let (TypeNode::Array(shape), Some(items)) = (&mut self.nodes[array.0 as usize], items) {
            shape.items = items;
        }
        Ok(array)
    }

    /// Structural equality of two lowered subtrees, ignoring field ids.
    fn same(&self, left: TypeId, right: TypeId) -> bool {
        match (&self.nodes[left.0 as usize], &self.nodes[right.0 as usize]) {
            (TypeNode::Scalar(a), TypeNode::Scalar(b)) => a == b,
            (TypeNode::Array(a), TypeNode::Array(b)) => {
                a.min_items == b.min_items
                    && a.max_items == b.max_items
                    && self.same(a.items, b.items)
            }
            (TypeNode::Object(a), TypeNode::Object(b)) => {
                a.reject_unknown == b.reject_unknown
                    && a.properties.len() == b.properties.len()
                    && a.properties.iter().zip(&b.properties).all(|(x, y)| {
                        x.name == y.name && x.required == y.required && self.same(x.ty, y.ty)
                    })
            }
            _ => false,
        }
    }
}

fn jmax_format_lookup(name: &str) -> Result<()> {
    jmax_format::registry::lookup(name).map(|_| ())
}
