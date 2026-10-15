//! Direct codec adapters with independent native validation.
//!
//! Applications opt into Serde by implementing [`DirectDeserialize`] and
//! [`DirectSerialize`] on their own wrappers. Their codecs must consume or emit
//! exactly one complete JSON value and check end of input. No general-purpose
//! JSON tree, optional dependency, or feature-dependent fallback is used here.

use alloc::vec::Vec;

use super::{
    BoundedSink, ByteSink, CompatOptions, CompatibilityValidator, DirectDeserialize,
    DirectSerialize, Error, ErrorCode, ErrorSource, JsonDecode, JsonEncode,
    LEGACY_FORMAT_VERSION, Limits, Payload, Result, ValidatedSlots,
};

/// Validate the complete input before invoking an application-owned codec.
///
/// The codec receives the original borrowed input, so its returned borrows never
/// depend on the temporary owned validation copy. Native validation is
/// authoritative even when the application's codec has permissive defaults.
pub fn decode_direct<'de, T: DirectDeserialize<'de>>(
    input: &'de [u8],
    validator: &mut dyn CompatibilityValidator,
    options: CompatOptions,
) -> Result<T> {
    let payload = prepare_input(input, options)?;
    validate_payload(&payload, validator, options.limits, ErrorSource::Decode)?;
    T::deserialize_json(input, options.limits)
        .map_err(|error| with_source(error, ErrorSource::Decode))
}

/// Encode directly into bounded storage, then independently validate its bytes.
///
/// Output preserves the codec's exact whitespace, escaping, and number spelling.
/// Nothing is published if encoding, native validation, or ownership checks fail.
pub fn encode_direct<T: DirectSerialize + ?Sized>(
    value: &T,
    validator: &mut dyn CompatibilityValidator,
    options: CompatOptions,
) -> Result<Payload> {
    let payload = encode_payload(options, |sink| value.serialize_json(sink))?;
    validate_payload(&payload, validator, options.limits, ErrorSource::Encode)?;
    Ok(payload)
}

/// Decode the caller's already validated slots without issuing a new proof.
///
/// Implementations of [`JsonDecode`] must resolve string references against the
/// slots' actual owners with checked bounds. This adapter does not rebase spans
/// or substitute a schema for the one used by the caller's validator.
pub fn decode_native<T: JsonDecode>(slots: &ValidatedSlots) -> Result<T> {
    T::decode_slots(slots)
}

/// Apply the same bounded encoding and validation boundary to a native encoder.
pub fn encode_native<T: JsonEncode + ?Sized>(
    value: &T,
    validator: &mut dyn CompatibilityValidator,
    options: CompatOptions,
) -> Result<Payload> {
    let payload = encode_payload(options, |sink| value.encode_json(sink))?;
    validate_payload(&payload, validator, options.limits, ErrorSource::Encode)?;
    Ok(payload)
}

fn with_source(mut error: Error, source: ErrorSource) -> Error {
    error.source = source;
    error
}

fn ensure_options(options: CompatOptions, source: ErrorSource) -> Result<()> {
    super::ensure_version(LEGACY_FORMAT_VERSION, options)
        .map_err(|error| with_source(error, source))
}

fn prepare_input(input: &[u8], options: CompatOptions) -> Result<Payload> {
    ensure_options(options, ErrorSource::Decode)?;
    if input.len() > options.limits.max_payload_bytes as usize {
        return Err(Error::new(
            ErrorCode::PayloadTooLarge,
            ErrorSource::Decode,
            0,
            "compatibility input byte limit exceeded",
        ));
    }
    core::str::from_utf8(input).map_err(|error| {
        Error::new(
            ErrorCode::InvalidUtf8,
            ErrorSource::Decode,
            // The payload ceiling above guarantees a representable offset.
            error.valid_up_to() as u32,
            "compatibility input is not valid UTF-8",
        )
    })?;

    let mut bytes = Vec::new();
    bytes.try_reserve_exact(input.len()).map_err(|_| {
        Error::new(
            ErrorCode::BudgetExhausted,
            ErrorSource::Decode,
            0,
            "compatibility input allocation failed",
        )
    })?;
    bytes.extend_from_slice(input);
    Ok(Payload::from_vec(bytes))
}

fn validate_payload(
    payload: &Payload,
    validator: &mut dyn CompatibilityValidator,
    limits: Limits,
    source: ErrorSource,
) -> Result<()> {
    // Retain the exact original owner while the validator consumes its clone.
    // A validator may return a distinct allocation with identical contents.
    let document = validator.validate_json(payload.clone(), limits)?;
    check_validation_result(
        payload.as_slice(),
        document.payload.as_slice(),
        document.proof.payload_len(),
        document.parts.decoded.len(),
        document.proof.decoded_len(),
        limits,
        source,
    )
}

// Check the untrusted return's metadata separately from proof issuance. A
// successful comparison does not create a proof or validate JSON by itself.
fn check_validation_result(
    expected: &[u8],
    owner: &[u8],
    proof_payload_len: u32,
    decoded_len: usize,
    proof_decoded_len: u32,
    limits: Limits,
    source: ErrorSource,
) -> Result<()> {
    if owner != expected
        || proof_payload_len as usize != owner.len()
        || proof_decoded_len as usize != decoded_len
    {
        return Err(Error::new(
            ErrorCode::InternalError,
            source,
            0,
            "compatibility validator returned inconsistent owners or proof lengths",
        ));
    }
    if decoded_len > limits.max_decoded_bytes as usize {
        return Err(Error::new(
            ErrorCode::DecodedArenaOverflow,
            source,
            0,
            "compatibility validator exceeded decoded arena limit",
        ));
    }
    Ok(())
}

fn encode_payload(
    options: CompatOptions,
    encode: impl FnOnce(&mut dyn ByteSink) -> Result<()>,
) -> Result<Payload> {
    ensure_options(options, ErrorSource::Encode)?;
    let maximum = options.max_output_bytes.min(options.limits.max_payload_bytes);
    let mut sink = EncodingSink {
        inner: BoundedSink::new(maximum),
        failure: None,
    };
    let result = encode(&mut sink);
    // Remember sink failures even if a faulty codec ignores or replaces them.
    // A previously emitted valid prefix must not hide an oversized write.
    if let Some(error) = sink.failure {
        return Err(error);
    }
    result.map_err(|error| with_source(error, ErrorSource::Encode))?;
    Ok(Payload::from_vec(sink.inner.into_vec()))
}

/// All output storage remains in the root's fallibly allocating bounded sink.
/// This wrapper records the first failure without allocating additional storage.
struct EncodingSink {
    inner: BoundedSink,
    failure: Option<Error>,
}

impl EncodingSink {
    fn record(&mut self, result: Result<()>) -> Result<()> {
        result.map_err(|error| {
            let error = with_source(error, ErrorSource::Encode);
            self.failure = Some(error.clone());
            error
        })
    }
}

impl ByteSink for EncodingSink {
    fn reserve(&mut self, additional: usize) -> Result<()> {
        if let Some(error) = &self.failure {
            return Err(error.clone());
        }
        let result = self.inner.reserve(additional);
        self.record(result)
    }

    fn write_bytes(&mut self, bytes: &[u8]) -> Result<()> {
        if let Some(error) = &self.failure {
            return Err(error.clone());
        }
        let result = self.inner.write_bytes(bytes);
        self.record(result)
    }

    fn len(&self) -> usize {
        self.inner.len()
    }
}
