//! Shared encoding, decoding, schema, and resource-budget interfaces.

use alloc::vec::Vec;

use crate::error::{Error, ErrorCode, ErrorSource, Result};
use crate::schema::{SchemaIr, ValidatedSlots};

/// An object-safe destination for encoded bytes.
pub trait ByteSink {
    /// Reserve room for at least `additional` bytes beyond the current length.
    fn reserve(&mut self, additional: usize) -> Result<()>;

    fn write_bytes(&mut self, bytes: &[u8]) -> Result<()>;

    fn write_byte(&mut self, b: u8) -> Result<()> {
        self.write_bytes(&[b])
    }

    fn len(&self) -> usize;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Vector writes reserve fallibly before appending, leaving the existing bytes
/// unchanged if capacity cannot be obtained. Both allocation failure and
/// capacity overflow are reported as `BudgetExhausted` from `Encode`.
impl ByteSink for Vec<u8> {
    fn reserve(&mut self, additional: usize) -> Result<()> {
        self.try_reserve(additional).map_err(|_| {
            Error::new(
                ErrorCode::BudgetExhausted,
                ErrorSource::Encode,
                u32::try_from(Vec::len(self)).unwrap_or(u32::MAX),
                "byte sink capacity reservation failed",
            )
        })
    }

    fn write_bytes(&mut self, bytes: &[u8]) -> Result<()> {
        <Self as ByteSink>::reserve(self, bytes.len())?;
        self.extend_from_slice(bytes);
        Ok(())
    }

    fn len(&self) -> usize {
        Vec::len(self)
    }
}

pub trait JsonDecode: Sized {
    fn decode_slots(slots: &ValidatedSlots) -> Result<Self>;
}

pub trait JsonEncode {
    fn encode_json(&self, sink: &mut dyn ByteSink) -> Result<()>;
}

pub trait SchemaProvider {
    fn schema_ir() -> SchemaIr;
    fn schema_fingerprint() -> u64;
}

/// Resource allowances carried across the shared ABI.
///
/// Enforcement and accounting belong to the operation consuming this value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResourceBudget {
    pub cpu_units: u32,
    pub retained_bytes: u32,
    pub timeout_ms: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::String;
    use crate::buffer::{Payload, ValidationProof};
    use crate::schema::{FieldId, SchemaFieldIr, SchemaId, SlotValue};
    use crate::syntax::ScalarKind;

    #[test]
    fn vector_sink_reserves_without_changing_length_or_contents() {
        let mut bytes = b"prefix".to_vec();
        ByteSink::reserve(&mut bytes, 64).unwrap();
        assert!(bytes.capacity() >= bytes.len() + 64);
        assert_eq!(bytes, b"prefix");
        ByteSink::reserve(&mut bytes, 0).unwrap();
        assert_eq!(ByteSink::len(&bytes), 6);
        assert!(!ByteSink::is_empty(&bytes));
    }

    #[test]
    fn vector_sink_appends_arbitrary_bytes_through_a_trait_object() {
        let mut bytes = Vec::new();
        {
            let sink: &mut dyn ByteSink = &mut bytes;
            assert!(sink.is_empty());
            sink.reserve(4).unwrap();
            assert_eq!(sink.len(), 0);
            sink.write_bytes(&[]).unwrap();
            assert!(sink.is_empty());
            sink.write_bytes(&[0, 0xff, b'a']).unwrap();
            sink.write_byte(b'\n').unwrap();
            assert_eq!(sink.len(), 4);
            assert!(!sink.is_empty());
        }
        assert_eq!(bytes, [0, 0xff, b'a', b'\n']);
        ByteSink::write_bytes(&mut bytes, b"tail").unwrap();
        assert_eq!(bytes, b"\0\xffa\ntail");
    }

    #[test]
    fn capacity_overflow_is_an_error_and_preserves_existing_bytes() {
        for initial in [&b""[..], &b"abc"[..]] {
            let mut bytes = initial.to_vec();
            let capacity = bytes.capacity();
            let error = ByteSink::reserve(&mut bytes, usize::MAX).unwrap_err();
            assert_eq!(error.code, ErrorCode::BudgetExhausted);
            assert_eq!(error.source, ErrorSource::Encode);
            assert_eq!(error.byte_offset, initial.len() as u32);
            assert_eq!(error.detail, "byte sink capacity reservation failed");
            assert_eq!(bytes.as_slice(), initial);
            assert_eq!(bytes.capacity(), capacity);
            // A failed reservation does not poison subsequent writes.
            ByteSink::write_byte(&mut bytes, b'!').unwrap();
            assert_eq!(bytes.last(), Some(&b'!'));
        }
    }

    struct LimitedSink {
        bytes: Vec<u8>,
        limit: usize,
    }

    impl ByteSink for LimitedSink {
        fn reserve(&mut self, additional: usize) -> Result<()> {
            if additional > self.limit.saturating_sub(self.bytes.len()) {
                return Err(Error::new(
                    ErrorCode::BudgetExhausted, ErrorSource::Encode,
                    u32::try_from(self.bytes.len()).unwrap_or(u32::MAX), "test sink limit",
                ));
            }
            ByteSink::reserve(&mut self.bytes, additional)
        }

        fn write_bytes(&mut self, bytes: &[u8]) -> Result<()> {
            self.reserve(bytes.len())?;
            ByteSink::write_bytes(&mut self.bytes, bytes)
        }

        fn len(&self) -> usize {
            self.bytes.len()
        }
    }

    #[test]
    fn default_single_byte_write_propagates_errors() {
        let mut sink = LimitedSink { bytes: Vec::new(), limit: 1 };
        assert!(sink.is_empty());
        sink.write_byte(b'a').unwrap();
        assert!(!sink.is_empty());
        let error = sink.write_byte(b'b').unwrap_err();
        assert_eq!(error, Error::new(
            ErrorCode::BudgetExhausted, ErrorSource::Encode, 1, "test sink limit",
        ));
        assert_eq!(sink.bytes, b"a");
        sink.write_bytes(&[]).unwrap();
    }

    #[derive(Debug, PartialEq, Eq)]
    struct Flag(bool);

    impl JsonEncode for Flag {
        fn encode_json(&self, sink: &mut dyn ByteSink) -> Result<()> {
            let json: &[u8] = if self.0 { b"{\"value\":true}" } else { b"{\"value\":false}" };
            sink.write_bytes(json)
        }
    }

    impl JsonDecode for Flag {
        fn decode_slots(slots: &ValidatedSlots) -> Result<Self> {
            if slots.proof.schema_fingerprint() != Self::schema_fingerprint() {
                return Err(Error::new(
                    ErrorCode::TypeMismatch, ErrorSource::Decode, 0, "unexpected flag schema",
                ));
            }
            match slots.slots.as_slice() {
                [SlotValue::Bool(value)] => Ok(Self(*value)),
                _ => Err(Error::new(
                    ErrorCode::TypeMismatch, ErrorSource::Decode, 0, "expected one boolean slot",
                )),
            }
        }
    }

    impl SchemaProvider for Flag {
        fn schema_ir() -> SchemaIr {
            SchemaIr {
                name: String::from("Flag"), schema_id: SchemaId(1),
                required_count: 1, reject_unknown: true,
                fields: alloc::vec![SchemaFieldIr {
                    field_id: FieldId(0), name: String::from("value"), required: true,
                    scalar_kind: Some(ScalarKind::True), format: None,
                    min_len: None, max_len: None, min_i64: None, max_i64: None,
                }],
            }
        }

        fn schema_fingerprint() -> u64 {
            0x464c_4147_0000_0001
        }
    }

    #[test]
    fn decode_encode_and_schema_traits_compose() {
        // This literal object has exactly the required field, whose value is
        // true. No string slots or decoded bytes are retained.
        let payload = Payload::from_slice(b"{\"value\":true}");
        // SAFETY: The 14-byte literal and Bool(true) slot satisfy Flag's schema
        // above by inspection. The decoded arena is empty and no spans exist.
        let proof = unsafe { ValidationProof::issue_unchecked(Flag::schema_fingerprint(), 14, 0) };
        let slots = ValidatedSlots {
            payload, decoded: Vec::new().into_boxed_slice(),
            slots: alloc::vec![SlotValue::Bool(true)], proof,
        };
        let flag = Flag::decode_slots(&slots).unwrap();
        assert_eq!(flag, Flag(true));
        let mut encoded = Vec::new();
        let encoder: &dyn JsonEncode = &flag;
        encoder.encode_json(&mut encoded).unwrap();
        assert_eq!(encoded, slots.payload.as_slice());
        let schema = Flag::schema_ir();
        assert_eq!(schema.schema_id, SchemaId(1));
        assert_eq!(schema.required_count, 1);
        assert!(schema.reject_unknown);
        assert_eq!(schema.fields[0].scalar_kind, Some(ScalarKind::True));
        assert_eq!(slots.proof.payload_len() as usize, encoded.len());
    }

    #[test]
    fn encoder_preserves_sink_failures() {
        let mut sink = LimitedSink { bytes: Vec::new(), limit: 0 };
        let error = Flag(false).encode_json(&mut sink).unwrap_err();
        assert_eq!(error, Error::new(
            ErrorCode::BudgetExhausted, ErrorSource::Encode, 0, "test sink limit",
        ));
        assert!(sink.is_empty());
        sink.limit = 15;
        Flag(false).encode_json(&mut sink).unwrap();
        assert_eq!(sink.bytes, b"{\"value\":false}");
    }

    #[test]
    fn resource_budget_preserves_all_allowances() {
        for value in [0, 1, u32::MAX] {
            let budget = ResourceBudget {
                cpu_units: value, retained_bytes: value, timeout_ms: value,
            };
            let copied = budget;
            assert_eq!(budget, copied);
            assert_eq!(copied.cpu_units, value);
            assert_eq!(copied.retained_bytes, value);
            assert_eq!(copied.timeout_ms, value);
        }
        let budget = ResourceBudget { cpu_units: 1, retained_bytes: 2, timeout_ms: 3 };
        assert_ne!(budget, ResourceBudget { cpu_units: 2, ..budget });
        assert_ne!(budget, ResourceBudget { retained_bytes: 3, ..budget });
        assert_ne!(budget, ResourceBudget { timeout_ms: 4, ..budget });
    }
}
