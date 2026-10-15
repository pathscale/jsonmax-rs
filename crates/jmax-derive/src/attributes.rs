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

#[cfg(test)]
mod tests {
    use super::*;

    // This fixture lexer constructs structural views only. Production always
    // uses the compiler lexer and original TokenTree values. Byte offsets in
    // fixtures also let diagnostics be asserted without compiler-only Spans.
    fn fixture(source: &str) -> Vec<Node> {
        fn scan(source: &str, cursor: &mut usize, closing: Option<char>) -> Vec<Node> {
            let mut nodes = Vec::new();
            while *cursor < source.len() {
                let at = *cursor;
                let ch = source[at..].chars().next().unwrap();
                if ch.is_whitespace() { *cursor += ch.len_utf8(); continue; }
                if Some(ch) == closing { *cursor += ch.len_utf8(); return nodes; }
                let delimiter = match ch {
                    '(' => Some((Delimiter::Parenthesis, ')')),
                    '[' => Some((Delimiter::Bracket, ']')),
                    '{' => Some((Delimiter::Brace, '}')),
                    _ => None,
                };
                if let Some((delimiter, close)) = delimiter {
                    *cursor += 1;
                    let children = scan(source, cursor, Some(close));
                    nodes.push(Node { at, kind: NodeKind::Group(delimiter, children) });
                    continue;
                }
                let raw_hashes = if ch == 'r' {
                    source[at + 1..].bytes().take_while(|byte| *byte == b'#').count()
                } else { 0 };
                let raw_string = ch == 'r' && source.as_bytes().get(at + 1 + raw_hashes) == Some(&b'"');
                if raw_string {
                    let body_start = at + 2 + raw_hashes;
                    let ending = format!("\"{}", "#".repeat(raw_hashes));
                    let end = source[body_start..].find(&ending).expect("closed raw fixture string");
                    *cursor = body_start + end + ending.len();
                    nodes.push(Node { at, kind: NodeKind::Literal(source[at..*cursor].into()) });
                } else if ch == '"' {
                    *cursor += 1;
                    loop {
                        let next = source[*cursor..].chars().next().expect("closed fixture string");
                        *cursor += next.len_utf8();
                        if next == '"' { break; }
                        if next == '\\' {
                            *cursor += source[*cursor..].chars().next().expect("fixture escape").len_utf8();
                        }
                    }
                    nodes.push(Node { at, kind: NodeKind::Literal(source[at..*cursor].into()) });
                } else if ch == '\'' && source.as_bytes().get(at + 2) == Some(&b'\'') {
                    *cursor += 3;
                    nodes.push(Node { at, kind: NodeKind::Literal(source[at..*cursor].into()) });
                } else if ch.is_ascii_digit() {
                    *cursor += 1;
                    while source[*cursor..].chars().next().is_some_and(|ch|
                        ch.is_ascii_alphanumeric() || ch == '_' || ch == '.')
                    { *cursor += 1; }
                    nodes.push(Node { at, kind: NodeKind::Literal(source[at..*cursor].into()) });
                } else if ch == '_' || ch.is_alphabetic() {
                    *cursor += ch.len_utf8();
                    if ch == 'r' && source.as_bytes().get(*cursor) == Some(&b'#') { *cursor += 1; }
                    while let Some(next) = source[*cursor..].chars().next() {
                        if next != '_' && !next.is_alphanumeric() { break; }
                        *cursor += next.len_utf8();
                    }
                    nodes.push(Node { at, kind: NodeKind::Ident(source[at..*cursor].into()) });
                } else {
                    assert!(!matches!(ch, ')' | ']' | '}'), "unexpected closing fixture group");
                    *cursor += ch.len_utf8();
                    let joint = source[*cursor..].chars().next().is_some_and(|next|
                        !next.is_alphanumeric() && !next.is_whitespace()
                            && !matches!(next, '_' | '(' | ')' | '[' | ']' | '{' | '}' | '"'));
                    nodes.push(Node { at, kind: NodeKind::Punct(ch, joint) });
                }
            }
            assert!(closing.is_none(), "unclosed fixture group");
            nodes
        }
        scan(source, &mut 0, None)
    }

    fn parse(source: &str, kind: DeriveKind) -> ParseResult<InputModel> {
        parse_model(&fixture(source), kind, &mut |path, _| Ok(fixture(path)))
    }

    fn record(source: &str) -> InputModel { parse(source, DeriveKind::Decode).unwrap() }

    fn fields(model: &InputModel) -> &[FieldModel] {
        match &model.shape { ShapeModel::Named(fields) => fields, _ => panic!("expected named fixture") }
    }

    fn variants(model: &InputModel) -> &[VariantModel] {
        match &model.shape { ShapeModel::Enum(variants) => variants, _ => panic!("expected enum fixture") }
    }

    fn spelling(nodes: &[Node]) -> String {
        nodes.iter().map(|node| match &node.kind {
            NodeKind::Ident(value) | NodeKind::Literal(value) => value.clone(),
            NodeKind::Punct(ch, _) => ch.to_string(),
            NodeKind::Group(delimiter, children) => {
                let (open, close) = match delimiter {
                    Delimiter::Parenthesis => ("(", ")"), Delimiter::Brace => ("{", "}"),
                    Delimiter::Bracket => ("[", "]"), Delimiter::None => ("", ""),
                };
                format!("{open} {} {close}", spelling(children))
            }
        }).collect::<Vec<_>>().join(" ")
    }

    fn assert_tokens(nodes: &[Node], expected: &str) {
        assert_eq!(spelling(nodes), spelling(&fixture(expected)));
    }

    fn assert_error(source: &str, kind: DeriveKind, contains: &str) -> ParseError {
        let first = parse(source, kind).unwrap_err();
        let second = parse(source, kind).unwrap_err();
        assert_eq!(first, second, "{source}");
        assert!(first.message.contains(contains), "{source}: {}", first.message);
        first
    }

    #[test]
    fn frozen_signatures_and_field_defaults_are_preserved() {
        let _: fn(TokenStream, DeriveKind) -> DeriveResult<DeriveInput> = parse_input;
        let _: fn(&DeriveInput) -> DeriveResult<TokenStream> = expand_definition_variant;
        let attrs = FieldAttrs::default();
        assert!(!attrs.skip && !attrs.default);
        assert!(attrs.rename.is_none() && attrs.format.is_none());
        assert!(attrs.min_len.is_none() && attrs.max_len.is_none());
        assert!(attrs.min_i64.is_none() && attrs.max_i64.is_none());
        assert_eq!(RenameRule::default(), RenameRule::Preserve);
    }

    #[test]
    fn default_containers_distinguish_empty_named_and_unit_shapes() {
        for kind in [DeriveKind::Decode, DeriveKind::Encode, DeriveKind::Schema] {
            for source in ["struct Empty;", "pub(crate) struct Empty {}"] {
                let model = parse(source, kind).unwrap();
                assert_eq!(model.ident.ident(), Some("Empty"));
                assert_tokens(&model.attrs.abi_path, "::jmax_abi");
                assert_tokens(&model.attrs.aot_path, "::jmax_schema_aot");
                assert_eq!(model.attrs.rename_all, RenameRule::Preserve);
                assert!(model.attrs.schema_name.is_none() && model.attrs.schema_id.is_none());
                assert!(model.attrs.reject_unknown);
                assert!(model.impl_generics.is_empty() && model.type_generics.is_empty());
                assert!(model.where_clause.is_empty());
                match &model.shape {
                    ShapeModel::Unit => assert!(source.ends_with(';')),
                    ShapeModel::Named(fields) => { assert!(source.ends_with('}')); assert!(fields.is_empty()); }
                    ShapeModel::Enum(_) => panic!("unexpected enum"),
                }
            }
        }
    }

    #[test]
    fn all_three_struct_derives_share_attributes_names_and_dense_slots() {
        let source = r###"
            #[doc = "consumer documentation"]
            #[jmax(abi_path = "crate::runtime::abi", aot_path = "::backend::aot",
                rename_all = "camelCase", schema_name = "Inventory", schema_id = 0xffff_ffffu32,
                reject_unknown = false)]
            pub(in crate::models) struct r#type<'a, T: Copy = u32, const N: usize = 3>
            where T: Sized, [u8; N]: Copy,
            {
                #[jmax(skip, default = true)] cache: &'a T,
                pub HTTPServerID: u32,
                #[jmax(default, min_i64 = -4, max_i64 = 9)] count: i64,
                #[jmax(rename = "東京\"key", format = "uuid", min_len = 0, max_len = 36)]
                title: ::core::option::Option<::alloc::string::String>,
                #[jmax(skip)] bytes: [u8; N],
                r#type: bool,
            }
        "###;
        for kind in [DeriveKind::Decode, DeriveKind::Encode, DeriveKind::Schema] {
            let model = parse(source, kind).unwrap();
            assert_eq!(model.ident.ident(), Some("r#type"));
            assert_tokens(&model.impl_generics, "<'a, T: Copy, const N: usize>");
            assert_tokens(&model.type_generics, "<'a, T, N>");
            assert_tokens(&model.where_clause, "where T: Sized, [u8; N]: Copy,");
            assert_tokens(&model.attrs.abi_path, "crate::runtime::abi");
            assert_tokens(&model.attrs.aot_path, "::backend::aot");
            assert_eq!(model.attrs.schema_name.as_deref(), Some("Inventory"));
            assert_eq!(model.attrs.schema_id, Some(u32::MAX));
            assert!(!model.attrs.reject_unknown);
            let fields = fields(&model);
            assert_eq!(fields.len(), 6);
            assert!(matches!(fields[0].ty, FieldType::Skipped));
            assert_tokens(&fields[0].rust_type, "&'a T");
            assert!(fields[0].attrs.default);
            assert_eq!(fields.iter().map(|field| field.slot).collect::<Vec<_>>(),
                [None, Some(0), Some(1), Some(2), None, Some(3)]);
            assert_eq!(fields[1].wire_name, "httpServerId");
            assert!(matches!(fields[1].ty, FieldType::Scalar(ScalarType::U32)));
            assert!(fields[2].attrs.default);
            assert_eq!((fields[2].attrs.min_i64, fields[2].attrs.max_i64), (Some(-4), Some(9)));
            assert_eq!(fields[3].wire_name, "東京\"key");
            assert!(matches!(fields[3].ty, FieldType::Optional(ScalarType::String)));
            assert_eq!(fields[3].attrs.format.as_deref(), Some("uuid"));
            assert_eq!((fields[3].attrs.min_len, fields[3].attrs.max_len), (Some(0), Some(36)));
            assert_tokens(&fields[4].rust_type, "[u8; N]");
            assert_eq!(fields[5].ident.ident(), Some("r#type"));
            assert_eq!(fields[5].wire_name, "type");
        }
    }

    #[test]
    fn exact_scalar_spellings_and_option_paths_cover_the_frozen_types() {
        let vectors = [
            ("bool", ScalarType::Bool), ("u32", ScalarType::U32), ("u64", ScalarType::U64),
            ("i32", ScalarType::I32), ("i64", ScalarType::I64), ("f64", ScalarType::F64),
            ("String", ScalarType::String), ("alloc::string::String", ScalarType::String),
            ("::alloc::string::String", ScalarType::String), ("std::string::String", ScalarType::String),
            ("::std::string::String", ScalarType::String), ("Decimal128", ScalarType::Decimal128),
            ("::jmax_abi::number::Decimal128", ScalarType::Decimal128),
            ("jmax_abi::number::Decimal128", ScalarType::Decimal128),
        ];
        for (source, expected) in vectors {
            let model = record(&format!("struct S {{ value: {source} }}"));
            assert!(matches!(fields(&model)[0].ty, FieldType::Scalar(found) if found == expected));
            for option in ["Option", "core::option::Option", "::core::option::Option",
                "std::option::Option", "::std::option::Option"] {
                for comma in ["", ","] {
                    let model = record(&format!("struct S {{ value: {option}<{source}{comma}> }}"));
                    assert!(matches!(fields(&model)[0].ty, FieldType::Optional(found) if found == expected));
                    assert_eq!(fields(&model)[0].slot, Some(0));
                }
            }
        }
    }

    #[test]
    fn decimal_uses_only_the_configured_abi_path_or_the_unqualified_name() {
        for ty in ["Decimal128", "crate::r#type::abi::number::Decimal128"] {
            let source = format!("#[jmax(abi_path = \"crate::r#type::abi\")] struct S {{ value: {ty} }}");
            let model = record(&source);
            assert!(matches!(fields(&model)[0].ty, FieldType::Scalar(ScalarType::Decimal128)));
        }
        for ty in ["other::number::Decimal128", "jmax_abi::Decimal128", "std::String",
            "unrelated::String", "crate::String", "core::primitive::u32", "::u32", "r#u32"] {
            assert_error(&format!("struct S {{ value: {ty} }}"), DeriveKind::Decode, "unsupported");
        }
        assert_error("#[jmax(abi_path = \"crate::abi\")] struct S { value: ::jmax_abi::number::Decimal128 }",
            DeriveKind::Encode, "unsupported");
    }

    #[test]
    fn borrows_nested_collections_aliases_and_malformed_options_are_errors() {
        for ty in ["&str", "&'a String", "&mut String", "u8", "u16", "usize", "isize", "f32",
            "Vec<u32>", "[u32; 3]", "(u32, bool)", "Box<String>", "Alias", "Option<Option<u32>>",
            "Option<Vec<u32>>", "custom::Option<u32>", "alloc::option::Option<u32>",
            "Option", "Option<>", "Option<u32, bool>", "Option<u32> + Send", "Option<&str>"] {
            for kind in [DeriveKind::Decode, DeriveKind::Encode, DeriveKind::Schema] {
                let error = parse(&format!("struct S {{ value: {ty} }}"), kind).unwrap_err();
                assert!(!error.message.is_empty(), "{ty}");
            }
        }
    }

    #[test]
    fn transparent_compiler_groups_are_classified_without_losing_original_type_structure() {
        let mut nodes = fixture("struct S { value: u32 }");
        if let NodeKind::Group(_, body) = &mut nodes[2].kind {
            let scalar = body.pop().unwrap();
            body.push(Node { at: scalar.at, kind: NodeKind::Group(Delimiter::None, vec![scalar]) });
        }
        let model = parse_model(&nodes, DeriveKind::Decode, &mut |path, _| Ok(fixture(path))).unwrap();
        let field = &fields(&model)[0];
        assert!(matches!(field.ty, FieldType::Scalar(ScalarType::U32)));
        assert!(matches!(field.rust_type[0].kind, NodeKind::Group(Delimiter::None, _)));
    }

    #[test]
    fn skipped_fields_preserve_arbitrary_types_and_boolean_flag_values() {
        let model = record("struct S<T> { #[jmax(skip, default)] a: T, \
            #[jmax(skip = true, default = false)] b: for<'a> fn(&'a str, u32) -> Result<T, T>, \
            #[jmax(skip)] c: <T as Trait<A, B>>::Assoc, \
            #[jmax(skip = false, default = false, rename = \"renamed\")] d: u32 }");
        let fields = fields(&model);
        assert_eq!(fields.len(), 4);
        assert!(fields[..3].iter().all(|field| matches!(field.ty, FieldType::Skipped) && field.slot.is_none()));
        assert_tokens(&fields[1].rust_type, "for<'a> fn(&'a str, u32) -> Result<T, T>");
        assert_tokens(&fields[2].rust_type, "<T as Trait<A, B>>::Assoc");
        assert!(fields[0].attrs.default && !fields[1].attrs.default);
        assert_eq!(fields[3].slot, Some(0));
        assert_eq!(fields[3].wire_name, "renamed");
        assert!(!fields[3].attrs.default && !fields[3].attrs.skip);
        for key in ["rename = \"x\"", "format = \"uuid\"", "min_len = 0", "max_len = 1",
            "min_i64 = 0", "max_i64 = 1"] {
            assert_error(&format!("struct S {{ #[jmax(skip, {key})] value: Arbitrary }}"),
                DeriveKind::Decode, "skip conflicts");
        }
        assert_error("struct S { #[jmax(skip, unknown = 1)] value: T }", DeriveKind::Decode, "unknown");
        assert_error("struct S { #[jmax(skip)] value: }", DeriveKind::Decode, "type must not be empty");
    }

    #[test]
    fn generics_keep_bounds_where_clauses_groups_and_arrows_but_strip_only_defaults() {
        let source = "struct S<'a: 'b, 'b, T: Trait<Item = Pair<A, B>> = Map<A, Vec<B>>, \
            U: Fn(u32, u64) -> Result<A, B> = Callback, \
            const N: usize = { choose::<A, B>(1, 2) }, const M: usize = 9,> \
            where for<'c> &'c T: Other<Pair<A, B>>, U: Fn() -> Result<A, B>, \
            { #[jmax(skip)] data: (&'a T, U, [u8; N], [u8; M]) }";
        let model = record(source);
        assert_tokens(&model.impl_generics, "<'a: 'b, 'b, T: Trait<Item = Pair<A, B>>, \
            U: Fn(u32, u64) -> Result<A, B>, const N: usize, const M: usize,>");
        assert_tokens(&model.type_generics, "<'a, 'b, T, U, N, M,>");
        assert_tokens(&model.where_clause,
            "where for<'c> &'c T: Other<Pair<A, B>>, U: Fn() -> Result<A, B>,");
        assert_tokens(&fields(&model)[0].rust_type, "(&'a T, U, [u8; N], [u8; M])");
        let unit = record("struct Unit<T = u32> where T: Copy;");
        assert!(matches!(unit.shape, ShapeModel::Unit));
        assert_tokens(&unit.impl_generics, "<T>");
        assert_tokens(&unit.type_generics, "<T>");
        assert_tokens(&unit.where_clause, "where T: Copy");
        let attributed = record("struct S<#[allow(unused)] T = u32, const r#type: usize = 1> { #[jmax(skip)] x: T }");
        assert_tokens(&attributed.impl_generics, "<#[allow(unused)] T, const r#type: usize>");
        assert_tokens(&attributed.type_generics, "<T, r#type>");
    }

    #[test]
    fn generic_and_declaration_error_paths_are_deterministic() {
        let vectors = [
            ("struct S<>;", "must not be empty"), ("struct S<,>;", "empty comma"),
            ("struct S<T,,U>;", "empty comma"), ("struct S<T = >;", "default must not be empty"),
            ("struct S<T bad>;", "unexpected token"), ("struct S<const N>;", "requires a type"),
            ("struct S<const N:>;", "nonempty type"), ("struct S<'a = 'b>;", "cannot have defaults"),
            ("struct S<T, T>;", "duplicate generic"),
            ("struct S<#[jmax(default)] T>;", "generic parameter"),
            ("struct S<T;", "unclosed"), ("struct S<T>>;", "where clause"),
            ("struct S where;", "nonempty where clause"), ("struct S; trailing", "expected a braced"),
            ("struct S { value }", "expected :"), ("struct S { value: }", "must not be empty"),
            ("struct S { : u32 }", "expected a named"),
            ("struct S { a: u32, r#a: bool }", "duplicate Rust field"),
            ("struct S { a: u32,, }", "empty comma"), ("struct S", "missing item body"),
            ("struct", "item identifier"), ("", "expected a struct"),
        ];
        for (source, message) in vectors { assert_error(source, DeriveKind::Decode, message); }
    }

    #[test]
    fn container_paths_accept_legal_keywords_raw_names_unicode_and_spacing() {
        for path in ["::runtime::abi", "runtime", "crate", "crate::abi", "self::abi",
            "super::abi", "super::super::abi", "r#type::r#async", "模块::abi", " :: runtime :: abi "] {
            let model = record(&format!("#[jmax(abi_path = \"{path}\", aot_path = \"{path}\")] struct S;"));
            assert_tokens(&model.attrs.abi_path, path);
            assert_tokens(&model.attrs.aot_path, path);
        }
        for path in ["", " ", "::", "crate::", "a:b", "a:::b", "a::::b", "a : : b", ": : a", "_", "a::_", "::crate::abi",
            "::self", "::super", "a::self", "a::super", "crate::super", "super::crate", "self::self",
            "r#self", "Self::abi", "a::<b>", "a()", "a + b", "$crate::abi", "fn::abi", "a::type"] {
            let source = format!("#[jmax(abi_path = \"{path}\")] struct S;");
            let error = parse(&source, DeriveKind::Decode).unwrap_err();
            assert!(error.message.contains("path") || error.message.contains("empty") || error.message.contains("keyword"),
                "{path}: {}", error.message);
        }
    }

    #[test]
    fn compiler_path_lexer_failures_are_not_replaced_with_default_paths() {
        let nodes = fixture("#[jmax(abi_path = \"bad\")] struct S;");
        let error = parse_model(&nodes, DeriveKind::Encode, &mut |source, at| {
            if source == "bad" { Err(failure(at, "path lexer rejected input")) }
            else { Ok(fixture(source)) }
        }).unwrap_err();
        assert_eq!(error.message, "path lexer rejected input");
        assert_eq!(error.at, 7);
    }

    #[test]
    fn attribute_duplicates_unknown_keys_misplacement_and_malformed_lists_fail() {
        let vectors = [
            ("#[jmax(schema_id = 1, schema_id = 2)] struct S;", "duplicate"),
            ("#[jmax(schema_id = 1)] #[jmax(schema_id = 2)] struct S;", "duplicate"),
            ("struct S { #[jmax(default)] #[jmax(default = false)] x: u32 }", "duplicate"),
            ("#[jmax(rename = \"x\")] struct S;", "container"),
            ("struct S { #[jmax(schema_name = \"X\")] x: u32 }", "field"),
            ("#[jmax(unknown)] struct S;", "unknown"),
            ("#[jmax] struct S;", "expected #[jmax"),
            ("#[jmax::nested] struct S;", "expected #[jmax"),
            ("#[jmax(schema_id)] struct S;", "requires a value"),
            ("#[jmax(schema_id =)] struct S;", "expected key = value"),
            ("#[jmax(schema_id(1))] struct S;", "expected key = value"),
            ("#[jmax(,)] struct S;", "empty comma"),
            ("#[jmax(schema_id = 1,,)] struct S;", "empty comma"),
            ("#[jmax(12 = 1)] struct S;", "attribute key"),
            ("#[jmax(schema_name = 1)] struct S;", "string literal"),
            ("#[jmax(schema_name = \"\")] struct S;", "must not be empty"),
            ("#[jmax(rename_all = \"snake\")] struct S;", "unsupported"),
            ("struct S { #[jmax(rename = \"\")] x: u32 }", "must not be empty"),
            ("struct S { #[jmax(format = \"\")] x: String }", "must not be empty"),
            ("#![jmax()] struct S;", "outer #[attribute]"),
        ];
        for (source, message) in vectors { assert_error(source, DeriveKind::Decode, message); }
        let model = record("#[jmax()] #[allow(dead_code)] struct S { #[jmax(default,)] x: u32, }");
        assert!(fields(&model)[0].attrs.default);
        let model = record("#[r#jmax(schema_id = 21)] struct S { #[r#jmax(r#default)] x: u32 }");
        assert_eq!(model.attrs.schema_id, Some(21));
        assert!(fields(&model)[0].attrs.default);
        assert_error("struct S { #[jmax(default, r#default)] x: u32 }", DeriveKind::Decode, "duplicate");
        for value in ["1", "\"true\"", "true false", "yes"] {
            assert_error(&format!("#[jmax(reject_unknown = {value})] struct S;"), DeriveKind::Decode, "true or false");
            assert_error(&format!("struct S {{ #[jmax(default = {value})] x: u32 }}"), DeriveKind::Decode, "true or false");
        }
        assert_error("#[jmax(reject_unknown)] struct S;", DeriveKind::Decode, "true or false");
    }

    #[test]
    fn integer_vectors_cover_bases_suffixes_and_every_target_boundary() {
        for (literal, expected) in [
            ("0", 0i128), ("1_000", 1000), ("0xff_u16", 255), ("0o7_7", 63), ("0b1010_u8", 10),
            ("-0x80i8", -128), ("127i8", 127), ("255u8", 255), ("-1u32", -1),
            ("2147483647i32", i128::from(i32::MAX)), ("-2147483648i32", i128::from(i32::MIN)),
            ("4294967295u32", i128::from(u32::MAX)),
            ("-9223372036854775808i64", i128::from(i64::MIN)),
            ("9223372036854775807i64", i128::from(i64::MAX)),
            ("-170141183460469231731687303715884105728i128", i128::MIN),
            ("170141183460469231731687303715884105727i128", i128::MAX),
            ("12usize", 12), ("-12isize", -12),
        ] {
            assert_eq!(integer_value(&fixture(literal), i128::MIN, i128::MAX).unwrap(), expected, "{literal}");
        }
        for literal in ["128i8", "-129i8", "256u8", "0x100u8", "1f64", "1.0", "1e2", "true", "'1'",
            "+1", "--1", "(1)", "1 + 1", "1 as i32", "SOME_CONST", "0b2", "0o8", "0x", "0b", "0o",
            "340282366920938463463374607431768211456u128", "170141183460469231731687303715884105728i128",
            "-170141183460469231731687303715884105729i128"] {
            assert!(integer_value(&fixture(literal), i128::MIN, i128::MAX).is_err(), "{literal}");
        }
        assert_eq!(integer_value(&fixture("4294967295"), 0, i128::from(u32::MAX)).unwrap(), i128::from(u32::MAX));
        for literal in ["4294967296", "-1", "-0"] {
            assert!(integer_value(&fixture(literal), 0, i128::from(u32::MAX)).is_err());
        }
        for literal in ["2147483648", "-2147483649"] {
            assert!(integer_value(&fixture(literal), i128::from(i32::MIN), i128::from(i32::MAX)).is_err());
        }
        for literal in ["9223372036854775808", "-9223372036854775809"] {
            assert!(integer_value(&fixture(literal), i128::from(i64::MIN), i128::from(i64::MAX)).is_err());
        }
    }

    #[test]
    fn attribute_numeric_boundaries_are_checked_before_narrowing() {
        let model = record("#[jmax(schema_id = 4294967295)] struct S { \
            #[jmax(min_len = 0, max_len = 4294967295)] text: String, \
            #[jmax(min_i64 = -9223372036854775808, max_i64 = 9223372036854775807)] n: i64 }");
        assert_eq!(model.attrs.schema_id, Some(u32::MAX));
        assert_eq!(fields(&model)[0].attrs.max_len, Some(u32::MAX));
        assert_eq!(fields(&model)[1].attrs.min_i64, Some(i64::MIN));
        assert_eq!(fields(&model)[1].attrs.max_i64, Some(i64::MAX));
        for key in ["min_len", "max_len"] {
            for value in ["4294967296", "-1"] {
                assert!(parse(&format!("struct S {{ #[jmax({key} = {value})] s: String }}"), DeriveKind::Schema).is_err());
            }
        }
        for key in ["min_i64", "max_i64"] {
            for value in ["9223372036854775808", "-9223372036854775809"] {
                assert!(parse(&format!("struct S {{ #[jmax({key} = {value})] n: i64 }}"), DeriveKind::Encode).is_err());
            }
        }
        assert!(parse("#[jmax(schema_id = 4294967296)] struct S;", DeriveKind::Decode).is_err());
        assert_eq!(next_slot(u32::MAX - 1, 12).unwrap(), u32::MAX);
        assert_eq!(next_slot(u32::MAX, 17).unwrap_err(), failure(17, "field slot count exceeds u32::MAX"));
    }

    #[test]
    fn constraint_compatibility_and_inverted_bounds_are_validated_for_every_derive() {
        for kind in [DeriveKind::Decode, DeriveKind::Encode, DeriveKind::Schema] {
            for scalar in ["bool", "u32", "u64", "i32", "i64", "f64", "Decimal128"] {
                for constraint in ["format = \"uuid\"", "min_len = 0", "max_len = 2"] {
                    assert_error(&format!("struct S {{ #[jmax({constraint})] x: {scalar} }}"), kind, "require String");
                }
            }
            for scalar in ["bool", "String", "f64", "Decimal128"] {
                assert_error(&format!("struct S {{ #[jmax(min_i64 = 0)] x: Option<{scalar}> }}"), kind, "integer scalar");
            }
            for scalar in ["u32", "u64", "i32", "i64"] {
                let model = parse(&format!("struct S {{ #[jmax(min_i64 = 1, max_i64 = 1)] x: Option<{scalar}> }}"), kind).unwrap();
                assert_eq!(fields(&model)[0].attrs.min_i64, Some(1));
            }
            assert_error("struct S { #[jmax(min_len = 2, max_len = 1)] x: String }", kind, "inverted");
            assert_error("struct S { #[jmax(min_i64 = 1, max_i64 = -1)] x: i64 }", kind, "inverted");
            let model = parse("struct S { #[jmax(min_len = 0, max_len = 0, format = \"uuid\")] x: Option<String> }", kind).unwrap();
            assert_eq!(fields(&model)[0].attrs.max_len, Some(0));
        }
    }

    #[test]
    fn rust_string_vectors_include_raw_unicode_ascii_escapes_and_continuations() {
        let vectors = [
            (r#""plain""#, "plain"), (r#""\"\\\'\0\n\r\t""#, "\"\\'\0\n\r\t"),
            (r#""\x00\x7f\u{1f980}\u{10_FFFF}""#, "\0\u{7f}🦀\u{10ffff}"),
            (r###"r##"raw\n"#quote"##"###, "raw\\n\"#quote"),
            (r#"r"東京""#, "東京"), ("\"a\\\n  \t b\"", "ab"),
            ("\"a\\\r\n  b\"", "ab"), ("\"a\r\nb\"", "a\nb"),
            ("r\"a\r\nb\"", "a\nb"), (r#""\u{0_0_4_1_}""#, "A"),
        ];
        for (literal, expected) in vectors { assert_eq!(decode_string(literal).unwrap(), expected, "{literal}"); }
        for literal in ["", "plain", "b\"bytes\"", "c\"cstr\"", "\"s\"suffix", "r#\"missing\"",
            r#""\q""#, r#""\x80""#, r#""\x0""#, r#""\u{}""#, r#""\u{_41}""#,
            r#""\u{d800}""#, r#""\u{110000}""#, r#""\u{0000000}""#, r#""\u1234""#,
            "\"bare\rCR\"", "r\"bare\rCR\"", "\"a\"b\""] {
            assert!(decode_string(literal).is_err(), "{literal}");
        }
        let many_hashes = format!("r{}\"x\"{}", "#".repeat(256), "#".repeat(256));
        assert!(decode_string(&many_hashes).is_err());
        let maximum_hashes = format!("r{}\"x\"{}", "#".repeat(255), "#".repeat(255));
        assert_eq!(decode_string(&maximum_hashes).unwrap(), "x");
        let model = record(r###"#[jmax(schema_name = r##"name"#quoted"##)] struct S { #[jmax(rename = "\u{6771}\u{4eac}")] x: bool }"###);
        assert_eq!(model.attrs.schema_name.as_deref(), Some("name\"#quoted"));
        assert_eq!(fields(&model)[0].wire_name, "東京");
    }

    #[test]
    fn casing_vectors_match_endpoint_model_acronym_digit_and_separator_boundaries() {
        let rules = [RenameRule::SnakeCase, RenameRule::CamelCase, RenameRule::PascalCase,
            RenameRule::ScreamingSnakeCase, RenameRule::KebabCase];
        let vectors = [
            ("HTTPServerID", ["http_server_id", "httpServerId", "HttpServerId", "HTTP_SERVER_ID", "http-server-id"]),
            ("HTTP2ServerID", ["http2_server_id", "http2ServerId", "Http2ServerId", "HTTP2_SERVER_ID", "http2-server-id"]),
            ("ABc", ["a_bc", "aBc", "ABc", "A_BC", "a-bc"]),
            ("aBCd", ["a_b_cd", "aBCd", "ABCd", "A_B_CD", "a-b-cd"]),
            ("x86_64CPU", ["x86_64_cpu", "x8664Cpu", "X8664Cpu", "X86_64_CPU", "x86-64-cpu"]),
            ("123ABCdef", ["123_ab_cdef", "123AbCdef", "123AbCdef", "123_AB_CDEF", "123-ab-cdef"]),
            ("foo42Bar7", ["foo42_bar7", "foo42Bar7", "Foo42Bar7", "FOO42_BAR7", "foo42-bar7"]),
        ];
        for (name, expected) in vectors {
            for (rule, expected) in rules.into_iter().zip(expected) {
                assert_eq!(convert_case(name, rule).unwrap(), expected);
            }
        }
        for separator in ['_', '-', ' ', '\t', '\n', '\r', '\x0c'] {
            let name = format!("{separator}{separator}HTTP{separator}Server{separator}ID{separator}");
            for (rule, expected) in rules.into_iter().zip(["http_server_id", "httpServerId", "HttpServerId", "HTTP_SERVER_ID", "http-server-id"]) {
                assert_eq!(convert_case(&name, rule).unwrap(), expected);
            }
        }
        for name in ["é", "東京", "a.b", "@", "___--- \t"] {
            assert_eq!(convert_case(name, RenameRule::Preserve).unwrap(), name);
            for rule in rules { assert!(convert_case(name, rule).is_err()); }
        }
        for rule in rules.into_iter().chain([RenameRule::Preserve]) {
            assert!(convert_case("", rule).is_err());
        }
        let long = "HTTPServerID_".repeat(2048);
        assert_eq!(convert_case(&long, RenameRule::SnakeCase).unwrap(), "http_server_id_".repeat(2048).trim_end_matches('_'));
    }

    #[test]
    fn raw_names_unicode_explicit_renames_and_wire_collisions_have_exact_behavior() {
        let model = record("struct r#type { r#match: bool, 東京: String }");
        assert_eq!(fields(&model)[0].ident.ident(), Some("r#match"));
        assert_eq!(fields(&model)[0].wire_name, "match");
        assert_eq!(fields(&model)[1].wire_name, "東京");
        assert_error("#[jmax(rename_all = \"snake_case\")] struct S { 東京: String }", DeriveKind::Decode, "ASCII");
        let model = record("#[jmax(rename_all = \"snake_case\")] struct S { #[jmax(rename = \"東京\")] 東京: String, #[jmax(skip)] 缓存: Unsupported }");
        assert_eq!(fields(&model)[0].wire_name, "東京");
        assert!(fields(&model)[1].slot.is_none());
        for source in [
            "#[jmax(rename_all = \"snake_case\")] struct S { HTTPServer: u32, http_server: bool }",
            "struct S { #[jmax(rename = \"same\")] a: u32, #[jmax(rename = \"same\")] b: bool }",
            "struct S { #[jmax(rename = \"東京\")] a: u32, #[jmax(rename = \"東京\")] b: bool }",
            "struct S { r#type: u32, #[jmax(rename = \"type\")] b: bool }",
        ] { assert_error(source, DeriveKind::Encode, "duplicate wire name"); }
        let model = record("struct S { #[jmax(skip)] hidden: T, #[jmax(rename = \"hidden\")] visible: bool }");
        assert_eq!(fields(&model)[1].slot, Some(0));
        for (rule, expected) in [("preserve", "HTTPServer"), ("snake_case", "http_server"),
            ("camelCase", "httpServer"), ("PascalCase", "HttpServer"),
            ("SCREAMING_SNAKE_CASE", "HTTP_SERVER"), ("kebab-case", "http-server")] {
            let model = record(&format!("#[jmax(rename_all = \"{rule}\")] struct S {{ HTTPServer: u32 }}"));
            assert_eq!(fields(&model)[0].wire_name, expected);
        }
    }

    #[test]
    fn definition_values_follow_declaration_order_and_allow_checked_explicit_resets() {
        let source = r###"
            #[repr(i32)] #[jmax(rename_all = "snake_case")]
            pub enum Status {
                First,
                HTTPReady = 0x7f_i32,
                #[jmax(value = -2147483648i32, rename = "最低", description = "minimum\nvalue")]
                r#type,
                Next,
                Maximum = 2147483647,
                #[jmax(value = 3, description = "")] Reset,
                Final,
            }
        "###;
        let model = parse(source, DeriveKind::DefinitionVariant).unwrap();
        let variants = variants(&model);
        assert_eq!(variants.iter().map(|variant| variant.value).collect::<Vec<_>>(),
            [0, 127, i32::MIN, i32::MIN + 1, i32::MAX, 3, 4]);
        assert_eq!(variants.iter().map(|variant| variant.wire_name.as_str()).collect::<Vec<_>>(),
            ["first", "http_ready", "最低", "next", "maximum", "reset", "final"]);
        assert_eq!(variants[2].ident.ident(), Some("r#type"));
        assert_eq!(variants[2].description, "minimum\nvalue");
        assert!(variants[5].description.is_empty());
        assert_eq!(implicit_value(None, 0).unwrap(), 0);
        assert_eq!(implicit_value(Some(i32::MAX - 1), 0).unwrap(), i32::MAX);
        assert_eq!(implicit_value(Some(i32::MAX), 5).unwrap_err().at, 5);
        assert!(parse("enum E { Last = 2147483647 }", DeriveKind::DefinitionVariant).is_ok());
        assert!(parse("enum E { Last = 2147483647, Reset = -1, Next }", DeriveKind::DefinitionVariant).is_ok());
    }

    #[test]
    fn definition_variant_rejects_conflicts_overflow_data_expressions_and_collisions() {
        let vectors = [
            ("enum E { A = 2147483647, B }", "overflows i32"),
            ("enum E { #[jmax(value = 2147483648)] A }", "must fit"),
            ("enum E { A = -2147483649 }", "must fit"),
            ("enum E { #[jmax(value = 1)] A = 1 }", "conflicts"),
            ("enum E { A(u32) }", "fieldless"), ("enum E { A() }", "fieldless"),
            ("enum E { A { x: u32 } }", "fieldless"),
            ("enum E { A = SOME_CONST }", "integer literal"),
            ("enum E { A = 1 + 2 }", "integer literal"),
            ("enum E { A = (1) }", "integer literal"),
            ("enum E { A = }", "integer literal"),
            ("enum E { A = 1 as i32 }", "integer literal"),
            ("enum E { A = +1 }", "integer literal"),
            ("enum E { A = 0, B = 0x0 }", "duplicate variant numeric"),
            ("enum E { A = -1, #[jmax(value = -1)] B }", "duplicate variant numeric"),
            ("enum E { #[jmax(rename = \"x\")] A, #[jmax(rename = \"x\")] B }", "duplicate variant wire"),
            ("#[jmax(rename_all = \"snake_case\")] enum E { HTTPReady, http_ready }", "duplicate variant wire"),
            ("enum E { A, r#A }", "duplicate Rust variant"),
            ("enum E { #[jmax(default)] A }", "variant"),
            ("enum E { #[jmax(value = 0, value = 1)] A }", "duplicate"),
            ("enum E { #[jmax(rename = \"\")] A }", "must not be empty"),
            ("enum E { #[jmax(description = 1)] A }", "string literal"),
            ("enum E<T> { A }", "generic"), ("enum E<'a> { A }", "generic"),
            ("enum E<const N: usize> { A }", "generic"),
            ("struct S;", "only fieldless enums"), ("union U { a: u32 }", "unions"),
        ];
        for (source, message) in vectors { assert_error(source, DeriveKind::DefinitionVariant, message); }
        for kind in [DeriveKind::Decode, DeriveKind::Encode, DeriveKind::Schema] {
            assert_error("enum E { A }", kind, "only named or unit structs");
            assert_error("struct S(u32);", kind, "tuple structs");
            assert_error("struct S();", kind, "tuple structs");
            assert_error("union U { value: u32 }", kind, "unions");
        }
    }

    fn bridge(model: &InputModel) -> String {
        let entries = variants(model).iter().map(|variant|
            (variant.wire_name.clone(), variant.value, variant.description.clone())).collect::<Vec<_>>();
        render_definition(model.ident.ident().unwrap(), &spelling(&model.where_clause), &entries).unwrap()
    }

    #[test]
    fn enum_bridge_has_exact_static_tuple_type_and_deterministic_ordered_source() {
        let model = parse("enum r#type { A, #[jmax(rename = \"b\\\"key\", value = -2, description = \"line\\n東京\")] B }",
            DeriveKind::DefinitionVariant).unwrap();
        let expected = concat!(
            "impl r#type  {\npub const JMAX_DEFINITION_VARIANTS: ",
            "&'static [(&'static str, i32, &'static str)] = &[\n",
            "(\"A\", 0i32, \"\"),\n",
            "(\"b\\\"key\", -2i32, \"line\\n東京\"),\n",
            "];\n}\n",
        );
        assert_eq!(bridge(&model), expected);
        assert_eq!(bridge(&model), bridge(&model));
        assert_eq!(expected.matches("pub const ").count(), 1);
        assert!(!expected.contains("traits::"));
        assert!(!expected.contains("enum r#type"));
        let empty = parse("enum Empty {}", DeriveKind::DefinitionVariant).unwrap();
        assert_eq!(bridge(&empty), "impl Empty  {\npub const JMAX_DEFINITION_VARIANTS: &'static [(&'static str, i32, &'static str)] = &[\n];\n}\n");
        let constrained = parse("enum E where u8: Copy { A }", DeriveKind::DefinitionVariant).unwrap();
        assert!(bridge(&constrained).starts_with("impl E where u8 : Copy {\n"));
    }

    #[test]
    fn bridge_copies_exactly_into_canonical_enum_variant_specs_and_escapes_hostile_text() {
        let entries = vec![
            (String::from("minimum"), i32::MIN, String::from("lowest")),
            (String::from("\"#; injected(); //\\\0東京"), i32::MAX, String::from("\n\r\t🦀")),
        ];
        let source = render_definition("Status", "", &entries).unwrap();
        for (name, value, description) in &entries {
            let literal = jmax_schema_aot::rust_string_literal(name);
            let description_literal = jmax_schema_aot::rust_string_literal(description);
            assert_eq!(decode_string(&literal).unwrap(), *name);
            assert_eq!(decode_string(&description_literal).unwrap(), *description);
            assert!(source.contains(&format!("({literal}, {value}i32, {description_literal}),\n")));
            let abi = jmax_abi::endpoint::EnumVariantSpec {
                name: name.clone(), value: *value, description: description.clone(),
            };
            assert_eq!((&abi.name, abi.value, &abi.description), (name, *value, description));
        }
        assert_eq!(source.matches("JMAX_DEFINITION_VARIANTS").count(), 1);
        assert!(source.contains("-2147483648i32"));
        assert!(source.contains("2147483647i32"));
    }

    #[test]
    fn renderer_defensively_rejects_empty_names_and_duplicate_bridge_entries() {
        let entry = (String::from("a"), 1, String::new());
        for (entries, index, message) in [
            (vec![(String::new(), 0, String::new())], 0, "must not be empty"),
            (vec![entry.clone(), (String::from("a"), 2, String::new())], 1, "wire name"),
            (vec![entry, (String::from("b"), 1, String::new())], 1, "numeric value"),
        ] {
            let error = render_definition("E", "", &entries).unwrap_err();
            assert_eq!(error.0, index);
            assert!(error.1.contains(message));
        }
    }

    #[test]
    fn diagnostics_select_the_offending_attribute_or_declaration() {
        let source = "struct S { #[jmax(min_len = 0)] value: u32 }";
        let error = assert_error(source, DeriveKind::Decode, "require String");
        assert_eq!(error.at, source.find("min_len").unwrap());
        let source = "struct S { #[jmax(min_len = 2, max_len = 1)] value: String }";
        let error = assert_error(source, DeriveKind::Encode, "inverted");
        assert_eq!(error.at, source.find("max_len").unwrap());
        let source = "struct S { #[jmax(rename = \"a\")] first: bool, a: u32 }";
        let error = assert_error(source, DeriveKind::Schema, "duplicate wire");
        assert_eq!(error.at, source.rfind("a:").unwrap());
        let source = "enum E { A = 2147483647, B }";
        let error = assert_error(source, DeriveKind::DefinitionVariant, "overflows");
        assert_eq!(error.at, source.find('B').unwrap());
        let source = "#[jmax(abi_path = \"a::self\")] struct S;";
        let error = parse(source, DeriveKind::Decode).unwrap_err();
        assert_eq!(error.at, source.find("abi_path").unwrap());
    }
}
