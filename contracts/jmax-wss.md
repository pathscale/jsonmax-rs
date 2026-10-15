# jmax-wss: frozen Wave 0 contract

Status: W0-ROOTS-7. The ABI in `jmax-abi` is authoritative. Module implementations belong to later waves; their absence in Wave 0 is intentional. Do not add placeholder modules or modify this root to make an earlier wave compile alone. Each module may use `use super::*;`. All shared types live in the root, with no root re-exports from child modules.

## Shared definitions and ownership

The exact root definitions, derives, constants, helper signatures, and implementations are reproduced in the root snapshot below. Only canonical `Payload`, `ValidatedSlots`, `ValidationProof`, `Limits`, `ResourceBudget`, and `WireEnvelopeView` are used. `Bytes` is the existing manifest's `bytes::Bytes`, adopted as `Payload::Bytes` without copying. The optional `nago-wss-transport` dependency supplies transport integration, not a second payload type. Core modules work with alloc and without std. The existing optional transport feature may require its own host runtime.

`ProtectedInput` asserts transport completeness and policy acceptance, not JSON validity. `ValidatedOutput` owns immutable bytes and a proof produced by `MessageValidator`; it has no public unchecked constructor. Cloning either type preserves its owner. Public frame payloads and validator return values are still checked for lengths, UTF-8, and applicable limits before publication. Proof metadata must not be treated as a substitute for running validation.

`MessageValidator::validate` must validate one complete JSON document, reject trailing content, apply every `Limits` field, validate unselected data, and publish spans only into its returned payload or decoded arena. Its configured schema fingerprint must be retained in its returned proof. Input and output paths call `ensure_validated_slots` and independently enforce policy byte limits. No module issues a proof itself.

## Wave 1: receive.rs

Exact public API, with root names in scope:

```rust
pub fn receive_bytes(bytes: Bytes, opcode: Opcode, policy: &WssPolicy) -> Result<ProtectedInput>;
pub fn receive_frame(state: &mut FragmentState, frame: IncomingFrame, policy: &WssPolicy) -> Result<ReceiveEvent>;
pub fn validate_input(input: ProtectedInput, validator: &mut dyn MessageValidator, policy: &WssPolicy) -> Result<ValidatedSlots>;
```

`receive_bytes` accepts only Text and policy-permitted Binary messages that the transport has already fully reassembled. It cannot be called on a fragment. It validates policy and byte lengths before adopting bytes, checks UTF-8 for both JSON data opcodes, and preserves the original Bytes owner. `receive_frame` delegates frame assembly and control processing to `policy::accept_frame`. `validate_input` runs the supplied validator on the complete protected owner and checks returned ownership/proof lengths. The decoded length must not exceed `limits.max_decoded_bytes`. No incomplete input can reach the validator.

Errors: `FramingError` for a non-data opcode; `TypeMismatch` for rejected binary JSON; `PayloadTooLarge` for either transport or syntax byte limits; `InvalidUtf8` with the first invalid byte offset; all validator errors propagate unchanged. Inconsistent returned owner/proof is `InternalError`. Local boundary errors have `ErrorSource::Transport` except UTF-8 validation, which uses `ErrorSource::Utf8`. ABI u32 length overflow is `PayloadTooLarge`.

## Wave 2: send.rs

```rust
pub fn prepare_output(payload: Payload, opcode: Opcode, validator: &mut dyn MessageValidator, policy: &WssPolicy) -> Result<ValidatedOutput>;
pub fn validate_output(output: &ValidatedOutput, policy: &WssPolicy) -> Result<()>;
```

`prepare_output` accepts only Text or permitted Binary, checks policy and limits, checks UTF-8, runs the configured validator on a clone of the immutable owner, checks proof/owner consistency, then constructs the private output fields. Only the owned payload and proof are retained. Allocation and decoded-arena accounting remain bounded by limits. `validate_output` checks the already validated private value's opcode, UTF-8, proof lengths, and current policy; it never grants a new schema proof. Its decoded proof length must fit the configured decoded limit even though decoded storage is not retained in an outbound frame. Transports must not expose arbitrary byte slices through the validated-output API. Error mapping matches receive; schema errors propagate. Output remains valid if the original application's buffer is dropped.

## Wave 3: policy.rs

```rust
pub fn validate_policy(policy: &WssPolicy) -> Result<()>;
pub fn accept_frame(state: &mut FragmentState, frame: IncomingFrame, policy: &WssPolicy) -> Result<ReceiveEvent>;
pub fn validate_close_payload(payload: &[u8]) -> Result<(Option<u16>, &str)>;
```

`validate_policy` rejects zero frame, message, fragment, or syntax payload limits and zero depth with `ConstraintViolation`; it permits a frame limit above the message limit because both are independently enforced. The effective assembled data bound is `min(max_message_bytes, limits.max_payload_bytes)`. Frame bytes are bounded by `max_frame_bytes`; control bytes additionally by `MAX_CONTROL_BYTES`. Fragment counts include the initial data frame and each Continuation, even empty fragments. Increment and accumulated lengths use checked arithmetic. Assembly uses fallible reservation and returns `BudgetExhausted` on allocation failure. A single final data frame preserves its Payload without copying; fragmented messages use an owned accumulated buffer.

Text/Binary can start only with no active fragmented message; Continuation requires one. A final data frame emits Message. A nonfinal data frame emits Pending. Ping/Pong are final, at most 125 bytes, emit their control event, and do not alter fragment state. Close is final, validates its payload, resets partial data, and permanently sets closed. After close, every further frame is `FramingError`. Transport masking, RSV bits, extensions, and wire opcode validity must be verified by nago-wss before constructing `IncomingFrame`; this abstraction does not carry those raw header fields.

A close payload of length zero yields `(None, "")`; length one is invalid. Otherwise the first two bytes are a big-endian code and the remainder is a UTF-8 reason. Accept only 1000..=1003, 1007..=1014, and 3000..=4999; do not accept reserved 1004, 1005, 1006, 1015, or 1016..=2999. Close event `reason` owns only reason bytes, excluding the status code. The reason is not JSON. UTF-8 validation of fragmented JSON occurs after assembly so split scalar encodings are allowed. BinaryPolicy::Json requires UTF-8 JSON in precisely the same way as Text.

On any data/control policy error, discard active message assembly with `reset_message`, without reopening a closed state. Errors are `FramingError` for protocol ordering/control violations, `PayloadTooLarge` for bytes, `BudgetExhausted` for fragment count/allocation, `InvalidUtf8` for text/reason, and `TypeMismatch` for binary rejection. State never publishes a partial message.

## Wave 4: bridge.rs

```rust
pub fn submit_input(dispatcher: &mut dyn OwnedDispatcher, connection: ConnectionId, input: ProtectedInput, budget: ResourceBudget) -> Result<()>;
pub fn forward_one<T: LocalTransport>(binding: &mut ReactorBinding<T>, dispatcher: &mut dyn OwnedDispatcher) -> Result<bool>;
pub fn close_connection<T: LocalTransport>(binding: &mut ReactorBinding<T>, connection: ConnectionId, code: u16, reason: &str) -> Result<()>;
```

`submit_input` rejects zero cpu_units or timeout_ms and input bytes greater than retained_bytes with `BudgetExhausted`, then transfers owned request data to the dispatcher. A dispatcher must account for payload, decoded arena, scratch, and output under the request budget. The bridge neither spawns an executor nor acquires a reactor implicitly. Endpoint and MCP routing are configured by the dispatcher, using the allowed wire/MCP dependencies and canonical ABI envelopes.

`forward_one` polls once. No completion returns false. One completion with an error propagates it without sending. Successful output is transferred once to the matching connection and returns true. `LocalTransport::try_send` must enqueue the entire immutable validated frame atomically or return an error without a partial frame; on failure this consumed completion is not silently retried. Retry/backpressure storage is the host's responsibility. Dispatchers return `BudgetExhausted` when full and `Cancelled` for cancelled work. Late completion for a closed connection is a transport error, never routed to a reused ID; hosts must allocate connection IDs without reuse while work remains outstanding.

`close_connection` validates the same legal code set and UTF-8 reason limit of 123 bytes before calling transport.close. ReactorBinding is !Send and !Sync through its Rc marker. Only owned BridgeRequest/BridgeCompletion values cross the CPU seam. All methods are nonblocking and return errors rather than waiting. The module dependency graph is receive -> policy, send -> policy, bridge -> root traits; policy never calls receive or send.

## Frozen root snapshot

This is the complete assigned root, included to make all field visibility, enum variants, defaults, and signatures exact.

```rust
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
```
