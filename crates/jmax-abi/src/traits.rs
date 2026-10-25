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
