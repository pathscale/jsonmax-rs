//! Explicit compatibility adapters. Legacy values never bypass native validation.
#![cfg_attr(not(feature = "std"), no_std)]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::{string::String, vec::Vec};
pub use jmax_abi::{
    buffer::Payload,
    endpoint::{EndpointField, EndpointSchemaSpec, EndpointType, EnumVariantSpec, ServiceSpec},
    error::{Error, ErrorCode, ErrorSource, Result},
    node::ValidatedDocument,
    schema::{SchemaIr, ValidatedSlots},
    span::{Span32, SpanSource, StrRef},
    syntax::Limits,
    traits::{ByteSink, JsonDecode, JsonEncode, SchemaProvider},
};

pub mod serde;
pub mod endpoint;
pub mod schema;
pub mod utilities;

pub const LEGACY_FORMAT_VERSION: u16 = 1;
pub const DEFAULT_MAX_OUTPUT_BYTES: u32 = 8 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LossPolicy { Reject, AllowDocumented }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompatOptions {
    pub legacy_version: u16,
    pub loss_policy: LossPolicy,
    pub limits: Limits,
    pub max_output_bytes: u32,
}

impl Default for CompatOptions {
    fn default() -> Self {
        Self {
            legacy_version: LEGACY_FORMAT_VERSION,
            loss_policy: LossPolicy::Reject,
            limits: Limits::strict_default(),
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompatibilityWarning { pub path: String, pub detail: String }

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conversion<T> { pub value: T, pub warnings: Vec<CompatibilityWarning> }

impl<T> Conversion<T> {
    pub fn exact(value: T) -> Self { Self { value, warnings: Vec::new() } }
    pub fn is_lossless(&self) -> bool { self.warnings.is_empty() }
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> Conversion<U> {
        Conversion { value: f(self.value), warnings: self.warnings }
    }
}

/// Application-owned wrappers implement this with their optional Serde codec.
/// Returned borrows may refer only to input, never to temporary parser storage.
pub trait DirectDeserialize<'de>: Sized {
    fn deserialize_json(input: &'de [u8], limits: Limits) -> Result<Self>;
}

/// Encode directly into the bounded native sink, without an intermediate DOM.
pub trait DirectSerialize {
    fn serialize_json(&self, sink: &mut dyn ByteSink) -> Result<()>;
}

/// Validates bytes independently of the optional compatibility codec.
pub trait CompatibilityValidator {
    fn validate_json(&mut self, input: Payload, limits: Limits) -> Result<ValidatedDocument>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LegacyEndpoint {
    pub version: u16,
    pub endpoint: EndpointSchemaSpec,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LegacyService {
    pub version: u16,
    pub id: u16,
    pub name: String,
    pub endpoints: Vec<LegacyEndpoint>,
}

#[derive(Clone, Debug)]
pub struct LegacySchema {
    pub version: u16,
    pub schema: SchemaIr,
    pub description: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LegacyDocumentation {
    pub version: u16,
    pub title: String,
    pub body: String,
}

/// The application's old schema parser supplies this seam; no format is guessed.
pub trait LegacySchemaCodec {
    fn decode_schema(&mut self, input: &[u8], limits: Limits) -> Result<LegacySchema>;
    fn encode_schema(&mut self, schema: &LegacySchema, sink: &mut dyn ByteSink) -> Result<()>;
}

/// Allocation failures and byte limits use the same ABI error as native sinks.
#[derive(Clone, Debug)]
pub struct BoundedSink {
    bytes: Vec<u8>,
    maximum: usize,
}

impl BoundedSink {
    pub fn new(maximum: u32) -> Self { Self { bytes: Vec::new(), maximum: maximum as usize } }
    pub fn as_slice(&self) -> &[u8] { &self.bytes }
    pub fn into_vec(self) -> Vec<u8> { self.bytes }
}

impl ByteSink for BoundedSink {
    fn reserve(&mut self, additional: usize) -> Result<()> {
        let total = self.bytes.len().checked_add(additional)
            .ok_or_else(|| compat_error(ErrorCode::PayloadTooLarge, "compatibility output length overflow"))?;
        if total > self.maximum {
            return Err(compat_error(ErrorCode::PayloadTooLarge, "compatibility output byte limit exceeded"));
        }
        self.bytes.try_reserve(additional)
            .map_err(|_| compat_error(ErrorCode::BudgetExhausted, "compatibility output allocation failed"))
    }
    fn write_bytes(&mut self, bytes: &[u8]) -> Result<()> {
        self.reserve(bytes.len())?;
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }
    fn len(&self) -> usize { self.bytes.len() }
}

pub(crate) fn compat_error(code: ErrorCode, detail: &'static str) -> Error {
    Error::new(code, ErrorSource::Decode, 0, detail)
}

pub(crate) fn ensure_version(version: u16, options: CompatOptions) -> Result<()> {
    if version != LEGACY_FORMAT_VERSION || options.legacy_version != LEGACY_FORMAT_VERSION {
        return Err(compat_error(ErrorCode::UnsupportedSchemaFeature, "unsupported legacy format version"));
    }
    Ok(())
}
