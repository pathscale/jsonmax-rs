//! Single-pass typed JSON readers whose every error comes from the schema VM.
//!
//! A native decoder only ever answers "accepted" or "I do not know" (`Fail`).
//! `decode_into` turns "I do not know" into the schema VM's exact error by
//! re-running the reference program, so no native routine can report a wrong
//! diagnostic, and no proof or capability is ever issued here.
#![forbid(unsafe_code)]

pub use crate::abi::error::{Error, ErrorCode, ErrorSource, Result};
pub use crate::abi::number::{Decimal128, DecimalPolicy, NumericPolicy};
pub use crate::schema_compile::ValidatedProgram;
pub use crate::format::FormatId;
pub use crate::schema_vm::VmLimits;

pub mod context;
pub mod decode;
pub mod reader;

pub use context::NativeContext;
pub use decode::{NativeDecode, decode_into};
pub use reader::Reader;

pub const CONTRACT_VERSION: u16 = 1;

/// "Not accepted, or not sure": the VM fallback reports the real outcome.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Fail;

pub type NativeResult<T> = core::result::Result<T, Fail>;
