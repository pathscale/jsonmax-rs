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

pub use buffer::{
    BackingIdentity, ByteOwner, ParsedMessage, Payload, SharedByteView, UnvalidatedSpan,
    ValidationCapability, ValidationProof,
};
pub use endpoint::{EndpointField, EndpointSchemaSpec, EndpointType, EnumVariantSpec, ServiceSpec};
pub use error::{Error, ErrorCode, ErrorSource, Result};
pub use node::{
    Container32, DocumentParts, NodeWord, TapeWord, ValidatedDocument, TAPE_AUX_LEN_MASK,
    TAPE_FLAG_ASCII, TAPE_FLAG_ESCAPED, TAPE_FLAG_EXTENDED, TAPE_FLAG_HAS_KEY,
    TAPE_FLAG_KEY_EXTENDED, TAPE_TAG_ARRAY, TAPE_TAG_FALSE, TAPE_TAG_KEY, TAPE_TAG_NULL,
    TAPE_TAG_NUMBER, TAPE_TAG_OBJECT, TAPE_TAG_STRING, TAPE_TAG_TRUE,
};
pub use number::{ConvertedNumber, Decimal128, DecimalPolicy, NumericPolicy};
pub use scan::{BackendId, BlockMasks, ClassifyFn, ScanCarry};
pub use schema::{
    CompileLimits, FieldId, InsnWord, KeyEntry, Opcode, SchemaFieldIr, SchemaId,
    SchemaImage, SchemaImageHeader, SchemaIr, Selection, SlotValue, StateId, ValidatedSlots,
};
pub use span::{NodeId, NumberLexKind, NumberRef, Span32, SpanId, SpanSource, StrRef};
pub use syntax::{
    CursorState, DuplicatePolicyOwner, Event, FramePhase, KeyMeta, Limits, ScalarKind, ScanStep,
    Scratch, SkipMode, StringMeta, TokenKind,
};
pub use traits::{ByteSink, JsonDecode, JsonEncode, ResourceBudget, SchemaProvider};
pub use wire::{EnvelopeKind, FramedHeader, WireEnvelopeView};
