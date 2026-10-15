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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_exports_preserve_canonical_types() {
        let span: span::Span32 = Span32::new(1, 2);
        let payload: buffer::Payload = Payload::from_slice(b"null");
        let error: error::Error = Error::new(
            ErrorCode::InvalidToken, ErrorSource::Lexer, span.offset, "token",
        );
        let result: Result<()> = Err(error);
        let carry: scan::ScanCarry = ScanCarry::default();
        let step: syntax::ScanStep = ScanStep::Emit(Event::Scalar {
            kind: ScalarKind::Null,
            span: Span32::new(0, 4),
            decoded: None,
        });
        assert_eq!(payload.as_slice(), b"null");
        assert_eq!(result.unwrap_err().byte_offset, 1);
        assert_eq!(carry.prev_in_string, 0);
        assert!(matches!(step, ScanStep::Emit(_)));
    }

    #[test]
    fn owned_abi_values_are_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Payload>();
        assert_send_sync::<ParsedMessage<StrRef>>();
        assert_send_sync::<Error>();
        assert_send_sync::<Scratch>();
        assert_send_sync::<ScanCarry>();
    }
}
