//! Shared, allocation-capable ABI for the jsonmax crates.
//!
//! References into messages use checked 32-bit spans. Validation proofs bind
//! validated values to their backing buffers and schema fingerprint.

#![no_std]

extern crate alloc;
#[cfg(feature = "std")]
extern crate std;

pub mod span;
pub mod buffer;
pub mod error;
pub mod scan;
pub mod syntax;
pub mod number;
pub mod node;
pub mod schema;
pub mod endpoint;
pub mod wire;
pub mod traits;

pub use buffer::{ParsedMessage, Payload, UnvalidatedSpan, ValidationProof};
pub use endpoint::{EndpointField, EndpointSchemaSpec, EndpointType, EnumVariantSpec, ServiceSpec};
pub use error::{Error, ErrorCode, ErrorSource, Result};
pub use node::{Container32, DocumentParts, NodeWord, ValidatedDocument};
pub use number::{ConvertedNumber, Decimal128, DecimalPolicy, NumericPolicy};
pub use scan::{BackendId, BlockMasks, ClassifyFn, ScanCarry};
pub use schema::{
    CompileLimits, FieldId, InsnWord, KeyEntry, Opcode, SchemaFieldIr, SchemaId,
    SchemaImage, SchemaImageHeader, SchemaIr, Selection, SlotValue, StateId, ValidatedSlots,
};
pub use span::{NodeId, NumberLexKind, NumberRef, Span32, SpanId, SpanSource, StrRef};
pub use syntax::{CursorState, Event, FramePhase, Limits, ScalarKind, ScanStep, Scratch, TokenKind};
pub use traits::{ByteSink, JsonDecode, JsonEncode, ResourceBudget, SchemaProvider};
pub use wire::{EnvelopeKind, FramedHeader, WireEnvelopeView};
