//! Shared attribute parsing and the fieldless-enum definition bridge.
//!
//! The compiler boundary retains original token trees, including their spans
//! and punctuation spacing. The parser walks an indexed structural view of
//! those trees; it never executes input or reconstructs user identifiers.

use super::*;
use std::collections::BTreeSet;

#[derive(Clone, Debug)]
struct Node {
    at: usize,
    kind: NodeKind,
}

#[derive(Clone, Debug)]
enum NodeKind {
    Ident(String),
    Literal(String),
    Punct(char, bool),
    Group(Delimiter, Vec<Node>),
}

impl Node {
    fn ident(&self) -> Option<&str> {
        match &self.kind { NodeKind::Ident(name) => Some(name), _ => None }
    }

    fn is_ident(&self, name: &str) -> bool { self.ident() == Some(name) }

    fn is_punct(&self, ch: char) -> bool {
        matches!(self.kind, NodeKind::Punct(found, _) if found == ch)
    }

    fn group(&self, delimiter: Delimiter) -> Option<&[Node]> {
        match &self.kind {
            NodeKind::Group(found, nodes) if *found == delimiter => Some(nodes),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ParseError {
    at: usize,
    message: String,
}

type ParseResult<T> = Result<T, ParseError>;

fn failure(at: usize, message: impl Into<String>) -> ParseError {
    ParseError { at, message: message.into() }
}

fn location(nodes: &[Node], fallback: usize) -> usize {
    nodes.first().map_or(fallback, |node| node.at)
}

fn import_tokens(stream: TokenStream, original: &mut Vec<TokenTree>) -> Vec<Node> {
    stream.into_iter().map(|token| {
        let at = original.len();
        original.push(token.clone());
        let kind = match token {
            TokenTree::Ident(ident) => NodeKind::Ident(ident.to_string()),
            TokenTree::Literal(literal) => NodeKind::Literal(literal.to_string()),
            TokenTree::Punct(punct) =>
                NodeKind::Punct(punct.as_char(), punct.spacing() == Spacing::Joint),
            TokenTree::Group(group) =>
                NodeKind::Group(group.delimiter(), import_tokens(group.stream(), original)),
        };
        Node { at, kind }
    }).collect()
}

fn retained_tokens(nodes: &[Node], original: &[TokenTree]) -> TokenStream {
    nodes.iter().map(|node| original[node.at].clone()).collect()
}

fn retained_ident(node: &Node, original: &[TokenTree]) -> DeriveResult<Ident> {
    match &original[node.at] {
        TokenTree::Ident(ident) => Ok(ident.clone()),
        token => Err(DeriveError::new(token.span(), "expected a Rust identifier")),
    }
}

#[derive(Clone, Debug)]
struct ContainerModel {
    abi_path: Vec<Node>,
    aot_path: Vec<Node>,
    rename_all: RenameRule,
    schema_name: Option<String>,
    schema_id: Option<u32>,
    reject_unknown: bool,
}

#[derive(Clone, Debug)]
struct FieldModel {
    ident: Node,
    rust_type: Vec<Node>,
    ty: FieldType,
    wire_name: String,
    slot: Option<u32>,
    attrs: FieldAttrs,
}

#[derive(Clone, Debug)]
struct VariantModel {
    ident: Node,
    wire_name: String,
    value: i32,
    description: String,
}

#[derive(Clone, Debug)]
enum ShapeModel { Named(Vec<FieldModel>), Unit, Enum(Vec<VariantModel>) }

#[derive(Clone, Debug)]
struct InputModel {
    ident: Node,
    impl_generics: Vec<Node>,
    type_generics: Vec<Node>,
    where_clause: Vec<Node>,
    attrs: ContainerModel,
    shape: ShapeModel,
}

/// Parse once for all derives. Only this adapter touches compiler token APIs.
pub(crate) fn parse_input(input: TokenStream, kind: DeriveKind) -> DeriveResult<DeriveInput> {
    let mut original = Vec::new();
    let nodes = import_tokens(input, &mut original);
    let fallback = nodes.first().map(|node| original[node.at].span())
        .unwrap_or_else(Span::call_site);
    let mut lex_path = |source: &str, at: usize| -> ParseResult<Vec<Node>> {
        let stream = source.parse::<TokenStream>()
            .map_err(|error| failure(at, format!("invalid Rust module path: {error}")))?;
        Ok(import_tokens(stream, &mut original))
    };
    let model = parse_model(&nodes, kind, &mut lex_path).map_err(|error| {
        DeriveError::new(original.get(error.at).map_or(fallback, TokenTree::span), error.message)
    })?;
    let span = original[model.ident.at].span();
    let shape = match model.shape {
        ShapeModel::Unit => ItemShape::UnitStruct,
        ShapeModel::Named(fields) => ItemShape::NamedStruct(fields.into_iter().map(|field| {
            Ok(FieldSpec {
                ident: retained_ident(&field.ident, &original)?,
                rust_type: retained_tokens(&field.rust_type, &original),
                ty: field.ty, wire_name: field.wire_name, slot: field.slot, attrs: field.attrs,
                span: original[field.ident.at].span(),
            })
        }).collect::<DeriveResult<Vec<_>>>()?),
        ShapeModel::Enum(variants) => ItemShape::Enum(variants.into_iter().map(|variant| {
            Ok(VariantSpec {
                ident: retained_ident(&variant.ident, &original)?,
                wire_name: variant.wire_name, value: variant.value,
                description: variant.description, span: original[variant.ident.at].span(),
            })
        }).collect::<DeriveResult<Vec<_>>>()?),
    };
    Ok(DeriveInput {
        ident: retained_ident(&model.ident, &original)?,
        impl_generics: retained_tokens(&model.impl_generics, &original),
        type_generics: retained_tokens(&model.type_generics, &original),
        where_clause: retained_tokens(&model.where_clause, &original),
        attrs: ContainerAttrs {
            abi_path: retained_tokens(&model.attrs.abi_path, &original),
            aot_path: retained_tokens(&model.attrs.aot_path, &original),
            rename_all: model.attrs.rename_all, schema_name: model.attrs.schema_name,
            schema_id: model.attrs.schema_id, reject_unknown: model.attrs.reject_unknown,
        },
        shape, span,
    })
}

fn unraw(name: &str) -> &str { name.strip_prefix("r#").unwrap_or(name) }

// A function arrow is punctuation, not a closing generic angle bracket.
fn is_arrow_end(nodes: &[Node], index: usize) -> bool {
    index > 0 && matches!(nodes[index - 1].kind, NodeKind::Punct('-', true))
}

fn angle_end(nodes: &[Node], start: usize) -> ParseResult<usize> {
    let mut depth = 0usize;
    for (index, node) in nodes.iter().enumerate().skip(start) {
        if node.is_punct('<') {
            depth += 1;
        } else if node.is_punct('>') && !is_arrow_end(nodes, index) {
            depth = depth.checked_sub(1)
                .ok_or_else(|| failure(node.at, "unmatched closing angle bracket"))?;
            if depth == 0 { return Ok(index); }
        }
    }
    Err(failure(nodes[start].at, "unclosed generic angle bracket"))
}

/// Groups are atomic. Only commas outside nested angles separate declarations.
fn list_ranges(nodes: &[Node]) -> ParseResult<Vec<(usize, usize)>> {
    let mut ranges = Vec::new();
    let mut start = 0;
    let mut depth = 0usize;
    for (index, node) in nodes.iter().enumerate() {
        if node.is_punct('<') {
            depth += 1;
        } else if node.is_punct('>') && !is_arrow_end(nodes, index) {
            depth = depth.checked_sub(1)
                .ok_or_else(|| failure(node.at, "unmatched closing angle bracket"))?;
        } else if depth == 0 && node.is_punct(',') {
            if start == index { return Err(failure(node.at, "empty comma-separated entry")); }
            ranges.push((start, index));
            start = index + 1;
        }
    }
    if depth != 0 {
        return Err(failure(location(nodes, 0), "unclosed generic angle bracket"));
    }
    if start < nodes.len() { ranges.push((start, nodes.len())); }
    Ok(ranges)
}

#[derive(Clone, Debug)]
struct Attribute {
    key: String,
    value: Option<Vec<Node>>,
    at: usize,
}

fn take_attributes(nodes: &[Node], cursor: &mut usize) -> ParseResult<Vec<Attribute>> {
    let mut attrs = Vec::new();
    while nodes.get(*cursor).is_some_and(|node| node.is_punct('#')) {
        let at = nodes[*cursor].at;
        let body = nodes.get(*cursor + 1).and_then(|node| node.group(Delimiter::Bracket))
            .ok_or_else(|| failure(at, "expected an outer #[attribute]"))?;
        *cursor += 2;
        if !body.first().and_then(Node::ident).is_some_and(|name| unraw(name) == "jmax") { continue; }
        let entries = if body.len() == 2 {
            body[1].group(Delimiter::Parenthesis)
        } else { None }.ok_or_else(|| failure(at, "expected #[jmax(...)]"))?;
        for (start, end) in list_ranges(entries)? {
            let entry = &entries[start..end];
            let key = entry[0].ident()
                .ok_or_else(|| failure(entry[0].at, "expected a jmax attribute key"))?;
            let value = if entry.len() == 1 { None } else {
                if !entry[1].is_punct('=') || entry.len() == 2 {
                    return Err(failure(entry[1].at, "expected key = value or a boolean flag"));
                }
                Some(entry[2..].to_vec())
            };
            attrs.push(Attribute { key: unraw(key).into(), value, at: entry[0].at });
        }
    }
    let mut seen = BTreeSet::new();
    for attr in &attrs {
        if !seen.insert(attr.key.as_str()) {
            return Err(failure(attr.at, format!("duplicate jmax attribute `{}`", attr.key)));
        }
    }
    Ok(attrs)
}

fn required_value(attr: &Attribute) -> ParseResult<&[Node]> {
    attr.value.as_deref().ok_or_else(||
        failure(attr.at, format!("jmax `{}` requires a value", attr.key)))
}

fn bool_value(attr: &Attribute, allow_bare: bool) -> ParseResult<bool> {
    match attr.value.as_deref() {
        None if allow_bare => Ok(true),
        Some([node]) if node.is_ident("true") => Ok(true),
        Some([node]) if node.is_ident("false") => Ok(false),
        _ => Err(failure(attr.at, format!("jmax `{}` requires true or false", attr.key))),
    }
}

fn string_value(attr: &Attribute, nonempty: bool) -> ParseResult<String> {
    let nodes = required_value(attr)?;
    let literal = match nodes {
        [Node { kind: NodeKind::Literal(literal), .. }] => literal,
        _ => return Err(failure(location(nodes, attr.at), "expected a Rust string literal")),
    };
    let value = decode_string(literal).map_err(|message|
        failure(location(nodes, attr.at), message))?;
    if nonempty && value.is_empty() {
        return Err(failure(attr.at, format!("jmax `{}` must not be empty", attr.key)));
    }
    Ok(value)
}

/// Decode Rust UTF-8 literals, including raw delimiters and line continuations.
/// Byte/C strings and suffixed literals are deliberately excluded.
fn decode_string(source: &str) -> Result<String, String> {
    let invalid = || String::from("expected an unsuffixed Rust UTF-8 string literal");
    if let Some(raw) = source.strip_prefix('r') {
        let hashes = raw.bytes().take_while(|byte| *byte == b'#').count();
        let rest = &raw[hashes..];
        let ending = format!("\"{}", "#".repeat(hashes));
        if hashes > 255 || !rest.starts_with('"') || !rest.ends_with(&ending)
            || rest.len() < 1 + ending.len()
        { return Err(invalid()); }
        let body = &rest[1..rest.len() - ending.len()];
        if body.contains(&ending) { return Err(invalid()); }
        return normalize_newlines(body);
    }
    let body = source.strip_prefix('"').and_then(|value| value.strip_suffix('"'))
        .ok_or_else(invalid)?;
    let mut chars = body.chars().peekable();
    let mut output = String::new();
    while let Some(ch) = chars.next() {
        match ch {
            '"' => return Err(invalid()),
            '\r' => {
                if chars.next() != Some('\n') { return Err(String::from("bare CR in string literal")); }
                output.push('\n');
            }
            '\\' => {
                let escaped = chars.next().ok_or_else(invalid)?;
                match escaped {
                    'n' => output.push('\n'), 'r' => output.push('\r'),
                    't' => output.push('\t'), '0' => output.push('\0'),
                    '\\' => output.push('\\'), '\'' => output.push('\''), '"' => output.push('"'),
                    'x' => {
                        let high = chars.next().and_then(|ch| ch.to_digit(16));
                        let low = chars.next().and_then(|ch| ch.to_digit(16));
                        match (high, low) {
                            (Some(high), Some(low)) if high < 8 =>
                                output.push(char::from((high * 16 + low) as u8)),
                            _ => return Err(String::from("invalid ASCII string escape")),
                        }
                    }
                    'u' => {
                        if chars.next() != Some('{') {
                            return Err(String::from("expected braces in Unicode string escape"));
                        }
                        let mut value = 0u32;
                        let mut digits = 0;
                        loop {
                            match chars.next() {
                                Some('}') if digits > 0 => break,
                                Some('_') if digits > 0 => continue,
                                Some(ch) if ch.is_ascii_hexdigit() && digits < 6 => {
                                    let digit = ch.to_digit(16)
                                        .ok_or_else(|| String::from("invalid Unicode string escape"))?;
                                    value = value * 16 + digit;
                                    digits += 1;
                                }
                                _ => return Err(String::from("invalid Unicode string escape")),
                            }
                        }
                        output.push(char::from_u32(value)
                            .ok_or_else(|| String::from("invalid Unicode scalar in string escape"))?);
                    }
                    '\n' | '\r' => {
                        if escaped == '\r' && chars.next() != Some('\n') {
                            return Err(String::from("bare CR in string continuation"));
                        }
                        while chars.peek().is_some_and(|ch| matches!(*ch, ' ' | '\t' | '\n' | '\r')) {
                            chars.next();
                        }
                    }
                    _ => return Err(String::from("invalid Rust string escape")),
                }
            }
            _ => output.push(ch),
        }
    }
    Ok(output)
}

fn normalize_newlines(source: &str) -> Result<String, String> {
    let mut output = String::with_capacity(source.len());
    let mut chars = source.chars();
    while let Some(ch) = chars.next() {
        if ch == '\r' {
            if chars.next() != Some('\n') { return Err(String::from("bare CR in string literal")); }
            output.push('\n');
        } else { output.push(ch); }
    }
    Ok(output)
}

fn integer_value(nodes: &[Node], minimum: i128, maximum: i128) -> ParseResult<i128> {
    let at = location(nodes, 0);
    let (negative, literal) = match nodes {
        [Node { kind: NodeKind::Literal(literal), .. }] => (false, literal),
        [minus, Node { kind: NodeKind::Literal(literal), .. }] if minus.is_punct('-') =>
            (true, literal),
        _ => return Err(failure(at, "expected an integer literal with an optional minus sign")),
    };
    if negative && minimum == 0 {
        return Err(failure(at, "expected a nonnegative integer literal"));
    }
    let (radix, digits) = if let Some(rest) = literal.strip_prefix("0x") { (16, rest) }
        else if let Some(rest) = literal.strip_prefix("0o") { (8, rest) }
        else if let Some(rest) = literal.strip_prefix("0b") { (2, rest) }
        else { (10, literal.as_str()) };
    let mut magnitude = 0u128;
    let mut count = 0;
    let mut suffix_start = digits.len();
    for (index, byte) in digits.bytes().enumerate() {
        if byte == b'_' { continue; }
        if let Some(digit) = char::from(byte).to_digit(radix) {
            magnitude = magnitude.checked_mul(u128::from(radix))
                .and_then(|value| value.checked_add(u128::from(digit)))
                .ok_or_else(|| failure(at, "integer literal overflows u128"))?;
            count += 1;
        } else { suffix_start = index; break; }
    }
    if count == 0 { return Err(failure(at, "integer literal has no digits")); }
    let suffix = &digits[suffix_start..];
    let suffix_limit = match suffix {
        "" | "usize" | "isize" => u128::MAX,
        "u8" => u128::from(u8::MAX), "u16" => u128::from(u16::MAX),
        "u32" => u128::from(u32::MAX), "u64" => u128::from(u64::MAX), "u128" => u128::MAX,
        "i8" => (i8::MAX as u128) + u128::from(negative),
        "i16" => (i16::MAX as u128) + u128::from(negative),
        "i32" => (i32::MAX as u128) + u128::from(negative),
        "i64" => (i64::MAX as u128) + u128::from(negative),
        "i128" => (i128::MAX as u128) + u128::from(negative),
        _ => return Err(failure(at, "expected an integer literal with a valid integer suffix")),
    };
    if magnitude > suffix_limit { return Err(failure(at, "integer literal exceeds its suffix range")); }
    let value = if negative && magnitude == (1u128 << 127) { i128::MIN } else {
        let value = i128::try_from(magnitude)
            .map_err(|_| failure(at, "integer literal is out of range"))?;
        if negative { -value } else { value }
    };
    if value < minimum || value > maximum {
        return Err(failure(at, format!("integer literal must fit {minimum}..={maximum}")));
    }
    Ok(value)
}

fn attr_u32(attr: &Attribute) -> ParseResult<u32> {
    let value = integer_value(required_value(attr)?, 0, i128::from(u32::MAX))?;
    u32::try_from(value).map_err(|_| failure(attr.at, "integer literal must fit u32"))
}

fn attr_i64(attr: &Attribute) -> ParseResult<i64> {
    let value = integer_value(required_value(attr)?, i128::from(i64::MIN), i128::from(i64::MAX))?;
    i64::try_from(value).map_err(|_| failure(attr.at, "integer literal must fit i64"))
}

fn unknown_attribute(attr: &Attribute, scope: &str) -> ParseError {
    let known = matches!(attr.key.as_str(), "abi_path" | "aot_path" | "rename_all"
        | "schema_name" | "schema_id" | "reject_unknown" | "rename" | "skip" | "default"
        | "format" | "min_len" | "max_len" | "min_i64" | "max_i64" | "value" | "description");
    failure(attr.at, if known {
        format!("jmax `{}` is not allowed on a {scope}", attr.key)
    } else { format!("unknown jmax attribute `{}`", attr.key) })
}

fn path_separator(nodes: &[Node], index: usize) -> bool {
    nodes.get(index).is_some_and(|node| matches!(node.kind, NodeKind::Punct(':', true)))
        && nodes.get(index + 1).is_some_and(|node| node.is_punct(':'))
}

fn path_segments(nodes: &[Node], at: usize) -> ParseResult<Vec<String>> {
    let mut cursor = 0;
    let absolute = path_separator(nodes, 0);
    if absolute { cursor = 2; }
    let mut segments = Vec::new();
    let mut super_prefix = true;
    loop {
        let node = nodes.get(cursor).ok_or_else(|| failure(at, "expected a nonempty Rust module path"))?;
        let name = node.ident().ok_or_else(|| failure(node.at, "expected a module path identifier"))?;
        let bare = unraw(name);
        if bare == "_" { return Err(failure(node.at, "underscore is not a module path identifier")); }
        let raw = name.starts_with("r#");
        let special = matches!(bare, "crate" | "self" | "super" | "Self");
        if special {
            let legal = !raw && !absolute && match bare {
                "crate" | "self" => segments.is_empty(),
                "super" => super_prefix,
                _ => false,
            };
            if !legal { return Err(failure(node.at, "path keyword is not legal in this position")); }
        } else if !raw && is_reserved_keyword(name) {
            return Err(failure(node.at, "Rust keyword must be escaped in a module path"));
        }
        super_prefix &= bare == "super";
        segments.push(name.into());
        cursor += 1;
        if cursor == nodes.len() { return Ok(segments); }
        if !path_separator(nodes, cursor)
        { return Err(failure(nodes[cursor].at, "expected :: between module path segments")); }
        cursor += 2;
    }
}

fn is_reserved_keyword(name: &str) -> bool {
    matches!(name, "as" | "async" | "await" | "break" | "const" | "continue" | "dyn"
        | "else" | "enum" | "extern" | "false" | "fn" | "for" | "if" | "impl" | "in"
        | "let" | "loop" | "match" | "mod" | "move" | "mut" | "pub" | "ref" | "return"
        | "static" | "struct" | "trait" | "true" | "type" | "unsafe" | "use" | "where"
        | "while" | "abstract" | "become" | "box" | "do" | "final" | "gen" | "macro"
        | "override" | "priv" | "try" | "typeof" | "unsized" | "virtual" | "yield")
}

fn container_attributes<F>(attrs: &[Attribute], at: usize, lex_path: &mut F)
    -> ParseResult<ContainerModel>
where F: FnMut(&str, usize) -> ParseResult<Vec<Node>>
{
    let mut model = ContainerModel {
        abi_path: lex_path("::jmax_abi", at)?, aot_path: lex_path("::jmax_schema_aot", at)?,
        rename_all: RenameRule::Preserve, schema_name: None, schema_id: None, reject_unknown: true,
    };
    for attr in attrs {
        match attr.key.as_str() {
            "abi_path" | "aot_path" => {
                let source = string_value(attr, true)?;
                let path = lex_path(&source, attr.at)?;
                path_segments(&path, attr.at).map_err(|error| failure(attr.at, error.message))?;
                if attr.key == "abi_path" { model.abi_path = path; } else { model.aot_path = path; }
            }
            "rename_all" => {
                model.rename_all = match string_value(attr, true)?.as_str() {
                    "preserve" => RenameRule::Preserve, "snake_case" => RenameRule::SnakeCase,
                    "camelCase" => RenameRule::CamelCase, "PascalCase" => RenameRule::PascalCase,
                    "SCREAMING_SNAKE_CASE" => RenameRule::ScreamingSnakeCase,
                    "kebab-case" => RenameRule::KebabCase,
                    _ => return Err(failure(attr.at, "unsupported jmax rename_all rule")),
                };
            }
            "schema_name" => model.schema_name = Some(string_value(attr, true)?),
            "schema_id" => model.schema_id = Some(attr_u32(attr)?),
            "reject_unknown" => model.reject_unknown = bool_value(attr, false)?,
            _ => return Err(unknown_attribute(attr, "container")),
        }
    }
    Ok(model)
}

fn field_attributes(attrs: &[Attribute]) -> ParseResult<FieldAttrs> {
    let mut model = FieldAttrs::default();
    for attr in attrs {
        match attr.key.as_str() {
            "rename" => model.rename = Some(string_value(attr, true)?),
            "skip" => model.skip = bool_value(attr, true)?,
            "default" => model.default = bool_value(attr, true)?,
            "format" => model.format = Some(string_value(attr, true)?),
            "min_len" => model.min_len = Some(attr_u32(attr)?),
            "max_len" => model.max_len = Some(attr_u32(attr)?),
            "min_i64" => model.min_i64 = Some(attr_i64(attr)?),
            "max_i64" => model.max_i64 = Some(attr_i64(attr)?),
            _ => return Err(unknown_attribute(attr, "field")),
        }
    }
    if model.skip {
        if let Some(conflict) = attrs.iter().find(|attr| !matches!(attr.key.as_str(), "skip" | "default")) {
            return Err(failure(conflict.at, "jmax skip conflicts with every field key except default"));
        }
    }
    if matches!((model.min_len, model.max_len), (Some(min), Some(max)) if min > max) {
        return Err(failure(attrs.iter().find(|attr| attr.key == "max_len").map_or(0, |attr| attr.at),
            "jmax length bounds are inverted"));
    }
    if matches!((model.min_i64, model.max_i64), (Some(min), Some(max)) if min > max) {
        return Err(failure(attrs.iter().find(|attr| attr.key == "max_i64").map_or(0, |attr| attr.at),
            "jmax integer bounds are inverted"));
    }
    Ok(model)
}

fn wire_name(name: &str, rename: Option<&str>, rule: RenameRule, at: usize) -> ParseResult<String> {
    let value = match rename {
        Some(value) => value.into(),
        None => convert_case(unraw(name), rule).map_err(|message| failure(at, message))?,
    };
    if value.is_empty() { return Err(failure(at, "wire name must not be empty")); }
    Ok(value)
}

/// Keep this algorithm identical to endpoint-model's ASCII word boundaries.
fn convert_case(name: &str, rule: RenameRule) -> Result<String, String> {
    if name.is_empty() { return Err(String::from("wire name must not be empty")); }
    if rule == RenameRule::Preserve { return Ok(name.into()); }
    if !name.is_ascii() { return Err(String::from("wire case conversion requires ASCII identifiers")); }
    let bytes = name.as_bytes();
    let mut output = String::with_capacity(name.len());
    let mut previous: Option<u8> = None;
    for (index, byte) in name.bytes().enumerate() {
        if byte == b'_' || byte == b'-' || byte.is_ascii_whitespace() { previous = None; continue; }
        if !byte.is_ascii_alphanumeric() {
            return Err(String::from("invalid character in wire case conversion"));
        }
        let starts_word = match previous {
            None => true,
            Some(previous) => byte.is_ascii_uppercase() && (previous.is_ascii_lowercase()
                || previous.is_ascii_digit() || (previous.is_ascii_uppercase()
                    && bytes.get(index + 1).is_some_and(u8::is_ascii_lowercase))),
        };
        let first_word = output.is_empty();
        if starts_word && !first_word {
            match rule {
                RenameRule::SnakeCase | RenameRule::ScreamingSnakeCase => output.push('_'),
                RenameRule::KebabCase => output.push('-'),
                _ => {}
            }
        }
        let rendered = match rule {
            RenameRule::Preserve => byte,
            RenameRule::ScreamingSnakeCase => byte.to_ascii_uppercase(),
            RenameRule::PascalCase if starts_word => byte.to_ascii_uppercase(),
            RenameRule::CamelCase if starts_word && !first_word => byte.to_ascii_uppercase(),
            _ => byte.to_ascii_lowercase(),
        };
        output.push(char::from(rendered));
        previous = Some(byte);
    }
    if output.is_empty() { return Err(String::from("wire case conversion requires at least one word")); }
    Ok(output)
}

fn scalar_type(nodes: &[Node], abi: &[Node]) -> ParseResult<ScalarType> {
    if let [node] = nodes {
        if let Some(inner) = node.group(Delimiter::None) { return scalar_type(inner, abi); }
        match node.ident() {
            Some("bool") => return Ok(ScalarType::Bool),
            Some("u32") => return Ok(ScalarType::U32), Some("u64") => return Ok(ScalarType::U64),
            Some("i32") => return Ok(ScalarType::I32), Some("i64") => return Ok(ScalarType::I64),
            Some("f64") => return Ok(ScalarType::F64),
            Some("String") => return Ok(ScalarType::String),
            Some("Decimal128") => return Ok(ScalarType::Decimal128),
            _ => {}
        }
    }
    let at = location(nodes, 0);
    let segments = path_segments(nodes, at).map_err(|_|
        failure(at, "unsupported field type; expected a supported owned scalar, not a borrow or collection"))?;
    let parts: Vec<&str> = segments.iter().map(String::as_str).collect();
    if parts == ["alloc", "string", "String"] || parts == ["std", "string", "String"] {
        return Ok(ScalarType::String);
    }
    let mut decimal = path_segments(abi, at)?;
    decimal.extend([String::from("number"), String::from("Decimal128")]);
    if segments == decimal { return Ok(ScalarType::Decimal128); }
    Err(failure(at, "unsupported scalar type or alias; arbitrary path suffixes are not recognized"))
}

fn field_type(nodes: &[Node], abi: &[Node]) -> ParseResult<FieldType> {
    if let [node] = nodes {
        if let Some(inner) = node.group(Delimiter::None) { return field_type(inner, abi); }
    }
    if let Some(open) = nodes.iter().position(|node| node.is_punct('<')) {
        let at = nodes[open].at;
        let path = path_segments(&nodes[..open], at)?;
        let parts: Vec<&str> = path.iter().map(String::as_str).collect();
        if parts != ["Option"] && parts != ["core", "option", "Option"]
            && parts != ["std", "option", "Option"]
        { return Err(failure(at, "unsupported collection or alias; only Option<Scalar> is supported")); }
        let close = angle_end(nodes, open)?;
        if close + 1 != nodes.len() { return Err(failure(nodes[close + 1].at, "unexpected tokens after Option type")); }
        let inner = &nodes[open + 1..close];
        let args = list_ranges(inner)?;
        if args.len() != 1 { return Err(failure(at, "Option requires exactly one supported scalar argument")); }
        let (start, end) = args[0];
        return scalar_type(&inner[start..end], abi).map(FieldType::Optional);
    }
    scalar_type(nodes, abi).map(FieldType::Scalar)
}

fn validate_constraints(ty: &FieldType, attrs: &FieldAttrs, source: &[Attribute], at: usize) -> ParseResult<()> {
    let scalar = match ty {
        FieldType::Scalar(scalar) | FieldType::Optional(scalar) => *scalar,
        FieldType::Skipped => return Ok(()),
    };
    if scalar != ScalarType::String {
        if let Some(attr) = source.iter().find(|attr|
            matches!(attr.key.as_str(), "format" | "min_len" | "max_len"))
        { return Err(failure(attr.at, "jmax format and length attributes require String")); }
    }
    if !matches!(scalar, ScalarType::U32 | ScalarType::U64 | ScalarType::I32 | ScalarType::I64)
        && (attrs.min_i64.is_some() || attrs.max_i64.is_some())
    {
        let at = source.iter().find(|attr| matches!(attr.key.as_str(), "min_i64" | "max_i64"))
            .map_or(at, |attr| attr.at);
        return Err(failure(at, "jmax integer bounds require an integer scalar"));
    }
    Ok(())
}

fn take_visibility(nodes: &[Node], cursor: &mut usize) {
    if nodes.get(*cursor).is_some_and(|node| node.is_ident("pub")) {
        *cursor += 1;
        if nodes.get(*cursor).and_then(|node| node.group(Delimiter::Parenthesis)).is_some() {
            *cursor += 1;
        }
    }
}

fn next_slot(slot: u32, at: usize) -> ParseResult<u32> {
    slot.checked_add(1).ok_or_else(|| failure(at, "field slot count exceeds u32::MAX"))
}

fn parse_fields(nodes: &[Node], container: &ContainerModel) -> ParseResult<Vec<FieldModel>> {
    let mut fields = Vec::new();
    let mut names = BTreeSet::new();
    let mut rust_names = BTreeSet::new();
    let mut slot = 0u32;
    for (start, end) in list_ranges(nodes)? {
        let entry = &nodes[start..end];
        let mut cursor = 0;
        let source = take_attributes(entry, &mut cursor)?;
        let attrs = field_attributes(&source)?;
        take_visibility(entry, &mut cursor);
        let ident = entry.get(cursor).filter(|node| node.ident().is_some())
            .ok_or_else(|| failure(location(entry, 0), "expected a named struct field"))?.clone();
        let name = ident.ident().unwrap_or("");
        if !rust_names.insert(unraw(name).to_owned()) {
            return Err(failure(ident.at, "duplicate Rust field identifier"));
        }
        cursor += 1;
        if !entry.get(cursor).is_some_and(|node| node.is_punct(':')) {
            return Err(failure(ident.at, "expected : after the field identifier"));
        }
        cursor += 1;
        let rust_type = &entry[cursor..];
        if rust_type.is_empty() { return Err(failure(ident.at, "field type must not be empty")); }
        let (ty, wire_name, selected_slot) = if attrs.skip {
            (FieldType::Skipped, unraw(name).into(), None)
        } else {
            let ty = field_type(rust_type, &container.abi_path)?;
            validate_constraints(&ty, &attrs, &source, ident.at)?;
            let wire = wire_name(name, attrs.rename.as_deref(), container.rename_all, ident.at)?;
            if !names.insert(wire.clone()) { return Err(failure(ident.at, format!("duplicate wire name `{wire}`"))); }
            let selected = slot;
            slot = next_slot(slot, ident.at)?;
            (ty, wire, Some(selected))
        };
        fields.push(FieldModel { ident, rust_type: rust_type.to_vec(), ty, wire_name,
            slot: selected_slot, attrs });
    }
    Ok(fields)
}

fn implicit_value(previous: Option<i32>, at: usize) -> ParseResult<i32> {
    match previous {
        None => Ok(0),
        Some(value) => value.checked_add(1)
            .ok_or_else(|| failure(at, "implicit variant value overflows i32")),
    }
}

fn parse_variants(nodes: &[Node], container: &ContainerModel) -> ParseResult<Vec<VariantModel>> {
    let mut variants = Vec::new();
    let mut names = BTreeSet::new();
    let mut rust_names = BTreeSet::new();
    let mut values = BTreeSet::new();
    let mut previous = None;
    for (start, end) in list_ranges(nodes)? {
        let entry = &nodes[start..end];
        let mut cursor = 0;
        let attrs = take_attributes(entry, &mut cursor)?;
        let mut rename = None;
        let mut explicit = None;
        let mut description = String::new();
        for attr in &attrs {
            match attr.key.as_str() {
                "rename" => rename = Some(string_value(attr, true)?),
                "description" => description = string_value(attr, false)?,
                "value" => {
                    let value = integer_value(required_value(attr)?, i128::from(i32::MIN), i128::from(i32::MAX))?;
                    explicit = Some(i32::try_from(value).map_err(|_| failure(attr.at, "variant value must fit i32"))?);
                }
                _ => return Err(unknown_attribute(attr, "variant")),
            }
        }
        let ident = entry.get(cursor).filter(|node| node.ident().is_some())
            .ok_or_else(|| failure(location(entry, 0), "expected a fieldless enum variant"))?.clone();
        let name = ident.ident().unwrap_or("");
        if !rust_names.insert(unraw(name).to_owned()) {
            return Err(failure(ident.at, "duplicate Rust variant identifier"));
        }
        cursor += 1;
        let value = if cursor == entry.len() {
            match explicit { Some(value) => value, None => implicit_value(previous, ident.at)? }
        } else {
            if !entry[cursor].is_punct('=') {
                return Err(failure(entry[cursor].at, "DefinitionVariant requires fieldless variants without data"));
            }
            if explicit.is_some() {
                return Err(failure(entry[cursor].at, "jmax value conflicts with a Rust discriminant"));
            }
            if cursor + 1 == entry.len() {
                return Err(failure(entry[cursor].at, "expected an integer literal after the Rust discriminant ="));
            }
            let parsed = integer_value(&entry[cursor + 1..], i128::from(i32::MIN), i128::from(i32::MAX))?;
            i32::try_from(parsed).map_err(|_| failure(ident.at, "variant discriminant must fit i32"))?
        };
        let wire = wire_name(name, rename.as_deref(), container.rename_all, ident.at)?;
        if !names.insert(wire.clone()) { return Err(failure(ident.at, format!("duplicate variant wire name `{wire}`"))); }
        if !values.insert(value) { return Err(failure(ident.at, format!("duplicate variant numeric value {value}"))); }
        previous = Some(value);
        variants.push(VariantModel { ident, wire_name: wire, value, description });
    }
    Ok(variants)
}

fn generic_parameters(nodes: &[Node]) -> ParseResult<(Vec<Node>, Vec<Node>)> {
    let mut implementation = vec![nodes[0].clone()];
    let mut arguments = vec![nodes[0].clone()];
    let inner = &nodes[1..nodes.len() - 1];
    let ranges = list_ranges(inner)?;
    if ranges.is_empty() { return Err(failure(nodes[0].at, "generic parameter list must not be empty")); }
    let mut names = BTreeSet::new();
    for (start, end) in ranges {
        let parameter = &inner[start..end];
        let mut cursor = 0;
        let attrs = take_attributes(parameter, &mut cursor)?;
        if let Some(attr) = attrs.first() { return Err(unknown_attribute(attr, "generic parameter")); }
        let lifetime = parameter.get(cursor).is_some_and(|node| node.is_punct('\''));
        let constant = parameter.get(cursor).is_some_and(|node| node.is_ident("const"));
        let argument_start = cursor;
        if lifetime || constant { cursor += 1; }
        let name = parameter.get(cursor).filter(|node| node.ident().is_some())
            .ok_or_else(|| failure(location(parameter, nodes[0].at), "expected a generic parameter name"))?;
        if !names.insert((lifetime, unraw(name.ident().unwrap_or("")).to_owned())) {
            return Err(failure(name.at, "duplicate generic parameter name"));
        }
        cursor += 1;
        if constant {
            if !parameter.get(cursor).is_some_and(|node| node.is_punct(':')) {
                return Err(failure(name.at, "const generic parameter requires a type"));
            }
        } else if parameter.get(cursor).is_some_and(|node| !node.is_punct(':') && !node.is_punct('=')) {
            return Err(failure(parameter[cursor].at, "unexpected token after generic parameter name"));
        }
        let mut depth = 0usize;
        let mut default = None;
        for (index, node) in parameter.iter().enumerate().skip(cursor) {
            if node.is_punct('<') { depth += 1; }
            else if node.is_punct('>') && !is_arrow_end(parameter, index) {
                depth = depth.checked_sub(1).ok_or_else(|| failure(node.at, "unmatched generic angle bracket"))?;
            } else if depth == 0 && node.is_punct('=') {
                default = Some(index);
                break;
            }
        }
        if let Some(index) = default {
            if lifetime { return Err(failure(parameter[index].at, "lifetime parameters cannot have defaults")); }
            if index + 1 == parameter.len() { return Err(failure(parameter[index].at, "generic default must not be empty")); }
        }
        let stop = default.unwrap_or(parameter.len());
        if constant && stop <= cursor + 1 {
            return Err(failure(name.at, "const generic parameter requires a nonempty type"));
        }
        implementation.extend_from_slice(&parameter[..stop]);
        if lifetime { arguments.extend_from_slice(&parameter[argument_start..cursor]); }
        else { arguments.push(name.clone()); }
        if let Some(comma) = inner.get(end) {
            implementation.push(comma.clone()); arguments.push(comma.clone());
        }
    }
    implementation.push(nodes[nodes.len() - 1].clone());
    arguments.push(nodes[nodes.len() - 1].clone());
    Ok((implementation, arguments))
}

fn where_clause(nodes: &[Node]) -> ParseResult<Vec<Node>> {
    if nodes.is_empty() { return Ok(Vec::new()); }
    if !nodes[0].is_ident("where") || nodes.len() == 1 {
        return Err(failure(nodes[0].at, "expected a nonempty where clause before the item body"));
    }
    list_ranges(&nodes[1..])?;
    Ok(nodes.to_vec())
}

fn parse_model<F>(nodes: &[Node], kind: DeriveKind, lex_path: &mut F) -> ParseResult<InputModel>
where F: FnMut(&str, usize) -> ParseResult<Vec<Node>>
{
    let mut cursor = 0;
    let source = take_attributes(nodes, &mut cursor)?;
    let attrs = container_attributes(&source, location(nodes, 0), lex_path)?;
    take_visibility(nodes, &mut cursor);
    let item = nodes.get(cursor).ok_or_else(|| failure(location(nodes, 0), "expected a struct or enum item"))?;
    let is_enum = match item.ident() {
        Some("struct") => false, Some("enum") => true,
        Some("union") => return Err(failure(item.at, "jmax derives do not support unions")),
        _ => return Err(failure(item.at, "expected a struct or enum item")),
    };
    if (kind == DeriveKind::DefinitionVariant) != is_enum {
        return Err(failure(item.at, if kind == DeriveKind::DefinitionVariant {
            "DefinitionVariant accepts only fieldless enums"
        } else { "JsonDecode, JsonEncode, and JsonSchema accept only named or unit structs" }));
    }
    cursor += 1;
    let ident = nodes.get(cursor).filter(|node| node.ident().is_some())
        .ok_or_else(|| failure(item.at, "expected the item identifier"))?.clone();
    cursor += 1;
    let (impl_generics, type_generics) = if nodes.get(cursor).is_some_and(|node| node.is_punct('<')) {
        if is_enum { return Err(failure(nodes[cursor].at, "generic DefinitionVariant enums are not supported")); }
        let end = angle_end(nodes, cursor)?;
        let parsed = generic_parameters(&nodes[cursor..=end])?;
        cursor = end + 1;
        parsed
    } else { (Vec::new(), Vec::new()) };
    let tail = &nodes[cursor..];
    if tail.first().and_then(|node| node.group(Delimiter::Parenthesis)).is_some() {
        return Err(failure(tail[0].at, "tuple structs are not supported"));
    }
    let last = tail.last().ok_or_else(|| failure(ident.at, "missing item body"))?;
    let prefix = &tail[..tail.len() - 1];
    let (shape, where_clause) = if let Some(body) = last.group(Delimiter::Brace) {
        let clause = where_clause(prefix)?;
        let shape = if is_enum { ShapeModel::Enum(parse_variants(body, &attrs)?) }
            else { ShapeModel::Named(parse_fields(body, &attrs)?) };
        (shape, clause)
    } else if last.is_punct(';') && !is_enum {
        (ShapeModel::Unit, where_clause(prefix)?)
    } else { return Err(failure(last.at, "expected a braced item body or a unit struct semicolon")); };
    Ok(InputModel { ident, impl_generics, type_generics, where_clause, attrs, shape })
}

/// The ABI bridge is an inherent constant, not an invented runtime trait.
pub(crate) fn expand_definition_variant(input: &DeriveInput) -> DeriveResult<TokenStream> {
    if !input.impl_generics.is_empty() || !input.type_generics.is_empty() {
        return Err(DeriveError::new(input.span, "generic DefinitionVariant enums are not supported"));
    }
    let variants = match &input.shape {
        ItemShape::Enum(variants) => variants,
        _ => return Err(DeriveError::new(input.span, "DefinitionVariant accepts only fieldless enums")),
    };
    let entries: Vec<_> = variants.iter().map(|variant|
        (variant.wire_name.clone(), variant.value, variant.description.clone())).collect();
    let source = render_definition(&input.ident.to_string(), &input.where_clause.to_string(), &entries)
        .map_err(|(index, message)| DeriveError::new(
            variants.get(index).map_or(input.span, |variant| variant.span), message))?;
    parse_generated(&source, input.span)
}

fn render_definition(ident: &str, clause: &str, variants: &[(String, i32, String)])
    -> Result<String, (usize, String)>
{
    let mut names = BTreeSet::new();
    let mut values = BTreeSet::new();
    let mut source = format!("impl {ident} {clause} {{\npub const JMAX_DEFINITION_VARIANTS: \
        &'static [(&'static str, i32, &'static str)] = &[\n");
    for (index, (name, value, description)) in variants.iter().enumerate() {
        if name.is_empty() { return Err((index, String::from("variant wire name must not be empty"))); }
        if !names.insert(name) { return Err((index, String::from("duplicate variant wire name"))); }
        if !values.insert(value) { return Err((index, String::from("duplicate variant numeric value"))); }
        source.push_str(&format!("({}, {value}i32, {}),\n",
            jmax_schema_aot::rust_string_literal(name), jmax_schema_aot::rust_string_literal(description)));
    }
    source.push_str("];\n}\n");
    Ok(source)
}
