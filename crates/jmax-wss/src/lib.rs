//! Owned WebSocket message boundaries and explicit local-reactor integration.
#![cfg_attr(not(feature = "std"), no_std)]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::{rc::Rc, vec::Vec};
use core::marker::PhantomData;
pub use bytes::Bytes;
pub use jmax_abi::{
    buffer::{Payload, ValidationProof},
    error::{Error, ErrorCode, ErrorSource, Result},
    schema::ValidatedSlots,
    syntax::Limits,
    traits::ResourceBudget,
    wire::WireEnvelopeView,
};

pub mod receive;
pub mod send;
pub mod policy;
pub mod bridge;

pub const MAX_CONTROL_BYTES: usize = 125;
pub const DEFAULT_MAX_MESSAGE_BYTES: u32 = 8 * 1024 * 1024;
pub const DEFAULT_MAX_FRAGMENTS: u32 = 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Opcode { Continuation, Text, Binary, Close, Ping, Pong }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinaryPolicy { Reject, Json }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WssPolicy {
    pub max_frame_bytes: u32,
    pub max_message_bytes: u32,
    pub max_fragments: u32,
    pub binary: BinaryPolicy,
    pub limits: Limits,
}

impl Default for WssPolicy {
    fn default() -> Self {
        Self {
            max_frame_bytes: DEFAULT_MAX_MESSAGE_BYTES,
            max_message_bytes: DEFAULT_MAX_MESSAGE_BYTES,
            max_fragments: DEFAULT_MAX_FRAGMENTS,
            binary: BinaryPolicy::Reject,
            limits: Limits::strict_default(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct IncomingFrame {
    pub opcode: Opcode,
    pub final_fragment: bool,
    pub payload: Payload,
}

/// Complete transport input. This type does not assert JSON validity.
#[derive(Clone, Debug)]
pub struct ProtectedInput {
    pub(crate) payload: Payload,
    pub(crate) opcode: Opcode,
}

impl ProtectedInput {
    pub fn payload(&self) -> &Payload { &self.payload }
    pub fn opcode(&self) -> Opcode { self.opcode }
    pub fn into_payload(self) -> Payload { self.payload }
}

/// Owned bytes published only after the configured validator succeeds.
#[derive(Clone, Debug)]
pub struct ValidatedOutput {
    pub(crate) payload: Payload,
    pub(crate) opcode: Opcode,
    pub(crate) proof: ValidationProof,
}

impl ValidatedOutput {
    pub fn payload(&self) -> &Payload { &self.payload }
    pub fn opcode(&self) -> Opcode { self.opcode }
    pub fn proof(&self) -> ValidationProof { self.proof }
    pub fn into_payload(self) -> Payload { self.payload }
}

/// Implementations must validate the complete document, including unselected fields.
pub trait MessageValidator {
    fn validate(&mut self, payload: Payload, limits: Limits) -> Result<ValidatedSlots>;
}

#[derive(Clone, Debug)]
pub enum ReceiveEvent {
    Pending,
    Message(ProtectedInput),
    Ping(Payload),
    Pong(Payload),
    Close { code: Option<u16>, reason: Payload },
}

#[derive(Debug, Default)]
pub struct FragmentState {
    pub(crate) opcode: Option<Opcode>,
    pub(crate) fragments: u32,
    pub(crate) bytes: Vec<u8>,
    pub(crate) closed: bool,
}

impl FragmentState {
    pub fn new() -> Self { Self::default() }
    pub fn is_fragmented(&self) -> bool { self.opcode.is_some() }
    pub fn retained_bytes(&self) -> usize { self.bytes.capacity() }
    pub fn is_closed(&self) -> bool { self.closed }
    /// Clears message assembly while retaining capacity and the close state.
    pub fn reset_message(&mut self) {
        self.opcode = None;
        self.fragments = 0;
        self.bytes.clear();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ConnectionId(pub u64);

#[derive(Debug)]
pub struct BridgeRequest {
    pub connection: ConnectionId,
    pub input: ProtectedInput,
    pub budget: ResourceBudget,
}

#[derive(Debug)]
pub struct BridgeCompletion {
    pub connection: ConnectionId,
    pub output: Result<ValidatedOutput>,
}

/// A transport implementation retains the frame if it accepts ownership.
pub trait LocalTransport {
    fn try_send(&mut self, connection: ConnectionId, output: ValidatedOutput) -> Result<()>;
    fn close(&mut self, connection: ConnectionId, code: u16, reason: &str) -> Result<()>;
}

/// CPU work owns its input; no reactor-local handle crosses this boundary.
pub trait OwnedDispatcher {
    fn submit(&mut self, request: BridgeRequest) -> Result<()>;
    fn poll_completion(&mut self) -> Result<Option<BridgeCompletion>>;
}

/// The Rc marker deliberately prevents moving a reactor binding across threads.
#[derive(Debug)]
pub struct ReactorBinding<T> {
    pub(crate) transport: T,
    pub(crate) local: PhantomData<Rc<()>>,
}

impl<T> ReactorBinding<T> {
    pub fn new(transport: T) -> Self { Self { transport, local: PhantomData } }
    pub fn transport(&self) -> &T { &self.transport }
    pub fn transport_mut(&mut self) -> &mut T { &mut self.transport }
    pub fn into_transport(self) -> T { self.transport }
}

pub(crate) fn transport_error(code: ErrorCode, detail: &'static str) -> Error {
    Error::new(code, ErrorSource::Transport, 0, detail)
}

pub(crate) fn ensure_payload_len(len: usize, maximum: u32) -> Result<u32> {
    let len = u32::try_from(len).map_err(|_| transport_error(ErrorCode::PayloadTooLarge, "payload exceeds ABI length"))?;
    if len > maximum {
        return Err(transport_error(ErrorCode::PayloadTooLarge, "WebSocket byte limit exceeded"));
    }
    Ok(len)
}

pub(crate) fn ensure_validated_slots(slots: &ValidatedSlots, expected: &[u8]) -> Result<()> {
    if slots.payload.as_slice() != expected
        || usize::try_from(slots.proof.payload_len()).ok() != Some(expected.len())
        || usize::try_from(slots.proof.decoded_len()).ok() != Some(slots.decoded.len())
    {
        return Err(transport_error(ErrorCode::InternalError, "validator returned inconsistent ownership or proof"));
    }
    Ok(())
}
