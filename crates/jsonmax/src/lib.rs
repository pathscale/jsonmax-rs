//! Native JSON facade. Query results and validation proofs have distinct types.
#![cfg_attr(not(feature = "std"), no_std)]
#![deny(unsafe_op_in_unsafe_fn)]
extern crate alloc;

use alloc::{string::String, vec::Vec};
pub use jmax_abi::{
    buffer::{ParsedMessage, Payload, UnvalidatedSpan, ValidationProof},
    endpoint::{EndpointField, EndpointSchemaSpec, EndpointType, ServiceSpec},
    error::{Error, ErrorCode, ErrorSource, Result},
    node::ValidatedDocument,
    number::{ConvertedNumber, Decimal128, DecimalPolicy, NumericPolicy},
    scan::BackendId,
    schema::{
        CompileLimits, FieldId, SchemaId, SchemaImage, SchemaIr, Selection, SlotValue,
        ValidatedSlots,
    },
    span::{NodeId, NumberRef, Span32, SpanSource, StrRef},
    syntax::{Limits, Scratch},
    traits::{ByteSink, JsonDecode, JsonEncode, ResourceBudget, SchemaProvider},
    wire::{EnvelopeKind, WireEnvelopeView},
};

pub mod encode;
pub mod endpoint;
pub mod parse;
pub mod query;

pub const CONTRACT_VERSION: u32 = 1;
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 16 * 1024 * 1024;
pub use endpoint::CompiledSchema;
/// Native documents use immutable packed tapes. `ValidatedDocument` is the
/// explicit legacy parallel-table representation.
pub use jmax_dom::{BorrowedDocument, Document, DomScratch, TapeDocument};
pub use jmax_schema_aot::JsonDecodeBorrowed;

/// Optional allocator availability. Enabling `rusty-alloc` does not install a
/// global allocator. A binary consumer can install it once in its crate root:
///
/// ```ignore
/// #[global_allocator]
/// static ALLOC: jsonmax::RustyAlloc = jsonmax::RustyAlloc;
/// ```
#[cfg(feature = "rusty-alloc")]
pub use rusty_alloc::RustyAlloc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParseOptions {
    pub limits: Limits,
    pub numeric: NumericPolicy,
}
impl Default for ParseOptions {
    fn default() -> Self {
        Self {
            limits: Limits::strict_default(),
            numeric: NumericPolicy::strict_default(),
        }
    }
}
/// Typed values remain paired with the exact slots and storage used to decode them.
#[derive(Debug)]
pub struct Decoded<T> {
    slots: ValidatedSlots,
    value: T,
}
impl<T> Decoded<T> {
    pub fn slots(&self) -> &ValidatedSlots {
        &self.slots
    }
    pub fn value(&self) -> &T {
        &self.value
    }
    pub fn payload(&self) -> &Payload {
        &self.slots.payload
    }
    pub fn proof(&self) -> ValidationProof {
        self.slots.proof
    }
    pub fn resolve_str(&self, reference: StrRef) -> Option<&str> {
        let bytes = match reference.source {
            SpanSource::RawPayload => reference.span.slice(self.slots.payload.as_slice())?,
            SpanSource::DecodedArena => reference.span.slice(&self.slots.decoded)?,
        };
        let text = core::str::from_utf8(bytes).ok()?;
        if text.chars().count() == reference.scalar_len as usize {
            Some(text)
        } else {
            None
        }
    }
    pub fn into_parts(self) -> (ValidatedSlots, T) {
        (self.slots, self.value)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueryMode {
    /// Validate the complete document, including unselected subtrees and its tail.
    Strict,
    /// Selected values are validated; skipped interiors may remain unvalidated.
    /// This mode never publishes a validation proof.
    Query,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueryOptions {
    pub parse: ParseOptions,
    pub mode: QueryMode,
    pub max_matches: u32,
}
impl Default for QueryOptions {
    fn default() -> Self {
        Self {
            parse: ParseOptions::default(),
            mode: QueryMode::Strict,
            max_matches: 1024,
        }
    }
}
#[derive(Clone, Debug)]
pub struct CompiledQuery {
    paths: Vec<String>,
    selector: query::SelectorTrie,
}
impl CompiledQuery {
    pub fn paths(&self) -> &[String] {
        &self.paths
    }
}
#[derive(Clone, Debug)]
pub struct QueryResult {
    payload: Payload,
    matches: Vec<Vec<Span32>>,
    fully_validated: bool,
}
impl QueryResult {
    pub fn payload(&self) -> &Payload {
        &self.payload
    }
    /// The outer index corresponds exactly to CompiledQuery::paths().
    pub fn matches(&self) -> &[Vec<Span32>] {
        &self.matches
    }
    pub fn fully_validated(&self) -> bool {
        self.fully_validated
    }
    pub fn raw_match(&self, path_index: usize, match_index: usize) -> Option<&[u8]> {
        self.matches
            .get(path_index)?
            .get(match_index)?
            .slice(self.payload.as_slice())
    }
}
/// Query matches borrowing the original input, with no input ownership copy.
/// Query mode may leave skipped interiors unvalidated and never issues a proof.
#[derive(Clone, Debug)]
pub struct BorrowedQueryResult<'a> {
    raw: &'a [u8],
    matches: Vec<Vec<Span32>>,
    fully_validated: bool,
}
impl<'a> BorrowedQueryResult<'a> {
    pub fn raw(&self) -> &'a [u8] {
        self.raw
    }
    pub fn matches(&self) -> &[Vec<Span32>] {
        &self.matches
    }
    pub fn fully_validated(&self) -> bool {
        self.fully_validated
    }
    pub fn raw_match(&self, path_index: usize, match_index: usize) -> Option<&'a [u8]> {
        self.matches
            .get(path_index)?
            .get(match_index)?
            .slice(self.raw)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EncodeOptions {
    pub max_output_bytes: usize,
}
impl Default for EncodeOptions {
    fn default() -> Self {
        Self {
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
        }
    }
}

/// Bounded growable sink. Failed writes leave the logical contents unchanged.
#[derive(Clone, Debug)]
pub struct OutputBuffer {
    bytes: Vec<u8>,
    max_bytes: usize,
}
impl OutputBuffer {
    pub const fn new(max_bytes: usize) -> Self {
        Self {
            bytes: Vec::new(),
            max_bytes,
        }
    }
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
    pub fn capacity(&self) -> usize {
        self.bytes.capacity()
    }
    pub fn into_vec(self) -> Vec<u8> {
        self.bytes
    }
    pub fn clear(&mut self) {
        self.bytes.clear();
    }
    pub fn max_bytes(&self) -> usize {
        self.max_bytes
    }
}
impl ByteSink for OutputBuffer {
    fn reserve(&mut self, additional: usize) -> Result<()> {
        let length = self.bytes.len().checked_add(additional).ok_or_else(|| {
            facade_error(
                ErrorCode::PayloadTooLarge,
                ErrorSource::Encode,
                "output size overflow",
            )
        })?;
        if length > self.max_bytes {
            return Err(facade_error(
                ErrorCode::PayloadTooLarge,
                ErrorSource::Encode,
                "output byte limit exceeded",
            ));
        }
        self.bytes.try_reserve(additional).map_err(|_| {
            facade_error(
                ErrorCode::BudgetExhausted,
                ErrorSource::Encode,
                "output allocation failed",
            )
        })
    }
    fn write_bytes(&mut self, bytes: &[u8]) -> Result<()> {
        self.reserve(bytes.len())?;
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }
    fn len(&self) -> usize {
        self.bytes.len()
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndpointDirection {
    Request,
    Response,
    Stream,
}
#[derive(Clone, Debug)]
pub struct EndpointCatalog {
    services: Vec<ServiceSpec>,
    entries: Vec<Option<endpoint::CatalogEntry>>,
}
impl EndpointCatalog {
    pub fn services(&self) -> &[ServiceSpec] {
        &self.services
    }
    pub(crate) fn entry(&self, code: u32) -> Option<&endpoint::CatalogEntry> {
        if self.entries.is_empty() {
            return None;
        }
        let mask = self.entries.len() - 1;
        let mut index = endpoint::catalog_hash(code) & mask;
        for _ in 0..self.entries.len() {
            match &self.entries[index] {
                Some(entry) if entry.code == code => return Some(entry),
                Some(_) => index = (index + 1) & mask,
                None => return None,
            }
        }
        None
    }
    pub fn endpoint(&self, code: u32) -> Option<&EndpointSchemaSpec> {
        let entry = self.entry(code)?;
        self.services
            .get(entry.service_index)?
            .endpoints
            .get(entry.endpoint_index)
    }
}
#[derive(Clone, Debug)]
pub struct EndpointMessage {
    pub endpoint_code: u32,
    pub direction: EndpointDirection,
    pub slots: ValidatedSlots,
}
pub fn facade_error(code: ErrorCode, source: ErrorSource, detail: &'static str) -> Error {
    Error::new(code, source, 0, detail)
}
