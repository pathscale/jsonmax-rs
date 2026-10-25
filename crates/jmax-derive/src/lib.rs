//! Derives for the jmax ABI. All implementation modules are private.

extern crate proc_macro;

use proc_macro::{Delimiter, Group, Ident, Literal, Punct, Spacing, Span, TokenStream, TokenTree};
use std::fmt;

mod attributes;
mod decode;
mod encode;
mod schema;

/// Internal, span-bearing diagnostics, never exported from this proc-macro crate.
#[derive(Clone, Debug)]
pub(crate) struct DeriveError {
    pub(crate) span: Span,
    pub(crate) message: String,
}

pub(crate) type DeriveResult<T> = core::result::Result<T, DeriveError>;

impl DeriveError {
    pub(crate) fn new(span: Span, message: impl Into<String>) -> Self {
        Self {
            span,
            message: message.into(),
        }
    }

    pub(crate) fn into_compile_error(self) -> TokenStream {
        let mut literal = Literal::string(&self.message);
        literal.set_span(self.span);
        let mut body = Group::new(
            Delimiter::Brace,
            TokenStream::from(TokenTree::Literal(literal)),
        );
        body.set_span(self.span);
        let mut bang = Punct::new('!', Spacing::Alone);
        bang.set_span(self.span);
        [
            TokenTree::Ident(Ident::new("compile_error", self.span)),
            TokenTree::Punct(bang),
            TokenTree::Group(body),
        ]
        .into_iter()
        .collect()
    }
}

impl fmt::Display for DeriveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DeriveKind {
    Decode,
    Encode,
    Schema,
    DefinitionVariant,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum RenameRule {
    #[default]
    Preserve,
    SnakeCase,
    CamelCase,
    PascalCase,
    ScreamingSnakeCase,
    KebabCase,
}

#[derive(Clone, Debug)]
pub(crate) struct ContainerAttrs {
    pub(crate) abi_path: TokenStream,
    pub(crate) aot_path: TokenStream,
    pub(crate) rename_all: RenameRule,
    pub(crate) schema_name: Option<String>,
    pub(crate) schema_id: Option<u32>,
    pub(crate) reject_unknown: bool,
}

impl Default for ContainerAttrs {
    fn default() -> Self {
        Self {
            abi_path: absolute_path("jmax_abi"),
            aot_path: absolute_path("jmax_schema_aot"),
            rename_all: RenameRule::Preserve,
            schema_name: None,
            schema_id: None,
            reject_unknown: true,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct FieldAttrs {
    pub(crate) rename: Option<String>,
    pub(crate) skip: bool,
    pub(crate) default: bool,
    pub(crate) format: Option<String>,
    pub(crate) min_len: Option<u32>,
    pub(crate) max_len: Option<u32>,
    pub(crate) min_i64: Option<i64>,
    pub(crate) max_i64: Option<i64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ScalarType {
    Bool,
    U32,
    U64,
    I32,
    I64,
    F64,
    String,
    Decimal128,
}

#[derive(Clone, Debug)]
pub(crate) enum FieldType {
    Scalar(ScalarType),
    Optional(ScalarType),
    Skipped,
}

#[derive(Clone, Debug)]
pub(crate) struct FieldSpec {
    pub(crate) ident: Ident,
    pub(crate) rust_type: TokenStream,
    pub(crate) ty: FieldType,
    pub(crate) wire_name: String,
    /// Dense index among non-skipped fields, or None for skipped fields.
    pub(crate) slot: Option<u32>,
    pub(crate) attrs: FieldAttrs,
    pub(crate) span: Span,
}

#[derive(Clone, Debug)]
pub(crate) struct VariantSpec {
    pub(crate) ident: Ident,
    pub(crate) wire_name: String,
    pub(crate) value: i32,
    pub(crate) description: String,
    pub(crate) span: Span,
}

#[derive(Clone, Debug)]
pub(crate) enum ItemShape {
    NamedStruct(Vec<FieldSpec>),
    UnitStruct,
    Enum(Vec<VariantSpec>),
}

#[derive(Clone, Debug)]
pub(crate) struct DeriveInput {
    pub(crate) ident: Ident,
    pub(crate) impl_generics: TokenStream,
    pub(crate) type_generics: TokenStream,
    pub(crate) where_clause: TokenStream,
    pub(crate) attrs: ContainerAttrs,
    pub(crate) shape: ItemShape,
    pub(crate) span: Span,
}

/// A path assembled as tokens avoids reparsing or dependence on a quoting crate.
pub(crate) fn absolute_path(name: &str) -> TokenStream {
    [
        TokenTree::Punct(Punct::new(':', Spacing::Joint)),
        TokenTree::Punct(Punct::new(':', Spacing::Alone)),
        TokenTree::Ident(Ident::new(name, Span::call_site())),
    ]
    .into_iter()
    .collect()
}

pub(crate) fn rust_string(value: &str) -> String {
    Literal::string(value).to_string()
}

/// Generated-source parsing failures are reported at the deriving item.
pub(crate) fn parse_generated(source: &str, span: Span) -> DeriveResult<TokenStream> {
    source
        .parse()
        .map_err(|error| DeriveError::new(span, format!("invalid generated Rust: {error}")))
}

fn expand(input: TokenStream, kind: DeriveKind) -> TokenStream {
    let result = attributes::parse_input(input, kind).and_then(|input| match kind {
        DeriveKind::Decode => decode::expand(&input),
        DeriveKind::Encode => encode::expand(&input),
        DeriveKind::Schema => schema::expand(&input),
        DeriveKind::DefinitionVariant => attributes::expand_definition_variant(&input),
    });
    match result {
        Ok(tokens) => tokens,
        Err(error) => error.into_compile_error(),
    }
}

#[proc_macro_derive(JsonDecode, attributes(jmax))]
pub fn derive_json_decode(input: TokenStream) -> TokenStream {
    expand(input, DeriveKind::Decode)
}

#[proc_macro_derive(JsonEncode, attributes(jmax))]
pub fn derive_json_encode(input: TokenStream) -> TokenStream {
    expand(input, DeriveKind::Encode)
}

#[proc_macro_derive(JsonSchema, attributes(jmax))]
pub fn derive_json_schema(input: TokenStream) -> TokenStream {
    expand(input, DeriveKind::Schema)
}

#[proc_macro_derive(DefinitionVariant, attributes(jmax))]
pub fn derive_definition_variant(input: TokenStream) -> TokenStream {
    expand(input, DeriveKind::DefinitionVariant)
}
