//! Deterministic differential and ownership harness contracts.
#![cfg_attr(not(feature = "std"), no_std)]
#![forbid(unsafe_code)]
extern crate alloc;

use alloc::{string::String, vec::Vec};
pub use jmax_abi::{
    buffer::Payload,
    error::{Error, ErrorCode, ErrorSource, Result},
    number::{ConvertedNumber, Decimal128, DecimalPolicy, NumericPolicy},
    scan::BackendId,
    schema::{SchemaImage, Selection},
    span::{Span32, SpanSource, StrRef},
    syntax::Limits,
};

pub mod syntax;
pub mod schema;
pub mod ownership;
pub mod numbers;

pub const CONTRACT_VERSION: u32 = 1;
pub const DEFAULT_SEED: u64 = 0x4a4d_4158_4655_5a5a;
pub const DEFAULT_MAX_INPUT_BYTES: usize = 1024 * 1024;
pub const DEFAULT_MAX_CASES: u32 = 10_000;
pub const MAX_RECORDED_FAILURES: u32 = 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RejectionComparison { AcceptanceOnly, Code, CodeAndOffset }
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FuzzConfig {
    pub seed: u64,
    pub max_cases: u32,
    pub max_input_bytes: usize,
    pub max_failures: u32,
    pub rejection_comparison: RejectionComparison,
}
impl Default for FuzzConfig {
    fn default() -> Self {
        Self { seed: DEFAULT_SEED, max_cases: DEFAULT_MAX_CASES, max_input_bytes: DEFAULT_MAX_INPUT_BYTES,
            max_failures: 32, rejection_comparison: RejectionComparison::AcceptanceOnly }
    }
}
impl FuzzConfig {
    pub fn validate(&self) -> Result<()> {
        if self.max_cases == 0 || self.max_input_bytes == 0 || self.max_input_bytes > u32::MAX as usize
            || self.max_failures == 0 || self.max_failures > MAX_RECORDED_FAILURES {
            return Err(harness_error(ErrorCode::ConstraintViolation, "invalid fuzz configuration"));
        }
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Accepted { canonical: Vec<u8> },
    Rejected { code: ErrorCode, byte_offset: u32 },
}
/// Canonical bytes encode the complete result under a common, documented policy.
/// An outer Err is harness failure. Invalid tested input is Outcome::Rejected.
pub trait SyntaxOracle {
    fn name(&self) -> &str;
    fn observe(&mut self, input: &[u8], limits: Limits) -> Result<Outcome>;
}
pub trait SchemaOracle {
    fn name(&self) -> &str;
    fn observe(&mut self, input: &Payload, schema: &SchemaImage, selection: &Selection, limits: Limits) -> Result<Outcome>;
}
#[derive(Clone, Debug)]
pub struct SchemaCase {
    pub name: String,
    pub payload: Payload,
    pub schema: SchemaImage,
    pub selection: Selection,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureKind { Acceptance, Value, ErrorCode, ErrorOffset, Ownership, Retention, Numeric }
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    pub case_index: u64,
    pub seed: u64,
    pub kind: FailureKind,
    pub input: Vec<u8>,
    pub detail: String,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FuzzReport {
    pub attempted: u64,
    pub agreed: u64,
    pub failures: Vec<Failure>,
    pub truncated: bool,
}
impl FuzzReport {
    pub fn is_success(&self) -> bool { self.attempted > 0 && self.attempted == self.agreed && self.failures.is_empty() && !self.truncated }
    /// Returns false once the configured failure storage budget is exhausted.
    pub fn record_failure(&mut self, failure: Failure, max_failures: u32) -> bool {
        if self.failures.len() >= max_failures.min(MAX_RECORDED_FAILURES) as usize {
            self.truncated = true;
            return false;
        }
        self.failures.push(failure);
        true
    }
}

#[derive(Clone, Debug)]
pub struct OwnershipSnapshot {
    pub payload: Payload,
    pub decoded: Vec<u8>,
    pub strings: Vec<StrRef>,
    pub raw_spans: Vec<Span32>,
    pub retained_bytes: u64,
}
pub trait OwnershipProbe {
    /// The snapshot owns all storage after this borrowed input has expired.
    fn inspect(&mut self, input: &[u8], limits: Limits) -> Result<OwnershipSnapshot>;
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OwnershipConfig {
    pub alignments: u8,
    pub max_retained_bytes: u64,
    pub limits: Limits,
}
impl Default for OwnershipConfig {
    fn default() -> Self {
        Self { alignments: 64, max_retained_bytes: 4 * DEFAULT_MAX_INPUT_BYTES as u64, limits: Limits::strict_default() }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NumberMode { Unsigned, Signed, Float, Decimal }
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NumberConfig {
    pub numeric: NumericPolicy,
    pub decimal: DecimalPolicy,
}
impl Default for NumberConfig {
    fn default() -> Self { Self { numeric: NumericPolicy::strict_default(), decimal: DecimalPolicy::spot_8() } }
}
#[derive(Clone, Debug, PartialEq)]
pub enum NumberOutcome { Value(ConvertedNumber), Rejected(ErrorCode) }
#[derive(Clone, Debug, PartialEq)]
pub struct NumberCase {
    pub name: String,
    pub lexeme: Vec<u8>,
    pub mode: NumberMode,
    pub expected: Option<NumberOutcome>,
}
pub trait NumberOracle {
    fn name(&self) -> &str;
    fn observe(&mut self, input: &[u8], mode: NumberMode, config: NumberConfig) -> Result<NumberOutcome>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeterministicRng { state: u64 }
impl DeterministicRng {
    pub const fn new(seed: u64) -> Self { Self { state: seed } }
    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut value = self.state;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        value ^ (value >> 31)
    }
    pub fn index(&mut self, upper_exclusive: usize) -> Option<usize> {
        if upper_exclusive == 0 { None } else { Some((self.next_u64() % upper_exclusive as u64) as usize) }
    }
}

/// Returns the first observable mismatch, in acceptance/value/code/offset order.
pub fn compare_outcomes(left: &Outcome, right: &Outcome, policy: RejectionComparison) -> Option<FailureKind> {
    match (left, right) {
        (Outcome::Accepted { canonical: left }, Outcome::Accepted { canonical: right }) => {
            if left == right { None } else { Some(FailureKind::Value) }
        }
        (Outcome::Rejected { code: left, byte_offset: lo }, Outcome::Rejected { code: right, byte_offset: ro }) => {
            match policy {
                RejectionComparison::AcceptanceOnly => None,
                _ if left != right => Some(FailureKind::ErrorCode),
                RejectionComparison::CodeAndOffset if lo != ro => Some(FailureKind::ErrorOffset),
                _ => None,
            }
        }
        _ => Some(FailureKind::Acceptance),
    }
}
pub fn harness_error(code: ErrorCode, detail: &'static str) -> Error {
    Error::new(code, ErrorSource::Decode, 0, detail)
}
