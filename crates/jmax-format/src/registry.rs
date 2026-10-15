//! Bounded, immutable dispatch for the frozen named-format registry.
//!
//! Names are exact, case-sensitive spellings. Inputs are decoded string bytes;
//! dispatch performs no parsing, normalization, allocation, or global mutation.

use crate::{
    binary, identity, uuid_ip, Base64Policy, Error, ErrorCode, ErrorSource,
    FormatFailure, FormatFailureKind, FormatId, FormatOptions, FormatResult, Result,
    MAX_FORMAT_NAME_BYTES,
};

/// Resolves one of the ten canonical [`FormatId::name`] spellings.
///
/// The byte-length budget is checked before name matching. Oversized names
/// produce `BudgetExhausted`; all other unknown names, including the empty
/// name, produce `UnsupportedSchemaFeature`. Errors carry `SchemaVm` as their
/// source and zero byte, schema, and field locations for callers to contextualize.
pub fn lookup(name: &str) -> Result<FormatId> {
    if name.len() > MAX_FORMAT_NAME_BYTES {
        return Err(Error::new(
            ErrorCode::BudgetExhausted,
            ErrorSource::SchemaVm,
            0,
            "format name limit exceeded",
        ));
    }

    match name {
        "uuid" => Ok(FormatId::Uuid),
        "ipv4" => Ok(FormatId::Ipv4),
        "ipv6" => Ok(FormatId::Ipv6),
        "ip" => Ok(FormatId::IpAddr),
        "base64" => Ok(FormatId::Base64),
        "base64url" => Ok(FormatId::Base64Url),
        "hex" => Ok(FormatId::Hex),
        "nanoid" => Ok(FormatId::NanoId),
        "blockchain-address" => Ok(FormatId::BlockchainAddress),
        "blockchain-transaction-hash" => Ok(FormatId::BlockchainTransactionHash),
        _ => Err(Error::new(
            ErrorCode::UnsupportedSchemaFeature,
            ErrorSource::SchemaVm,
            0,
            "unsupported format name",
        )),
    }
}

/// Validates decoded string bytes using the selected format and its options.
///
/// The input budget is inclusive and checked before any format-specific work.
/// Its failure index is the slice length, saturated to `u32::MAX`. Only hex,
/// base64 padding, and NanoId length have configurable format policies; address
/// and transaction-hash policies remain fixed regardless of `options.hex`.
/// Successful binary validation discards the decoded byte count.
pub fn validate(id: FormatId, input: &[u8], options: &FormatOptions) -> FormatResult<()> {
    // Checked conversion also rejects slices longer than u32::MAX without
    // truncating their length or narrowing the configured budget to usize.
    if u32::try_from(input.len()).map_or(true, |len| len > options.max_input_bytes) {
        return Err(FormatFailure::new(FormatFailureKind::Limit, input.len()));
    }

    match id {
        FormatId::Uuid => uuid_ip::validate_uuid(input),
        FormatId::Ipv4 => uuid_ip::validate_ipv4(input),
        FormatId::Ipv6 => uuid_ip::validate_ipv6(input),
        FormatId::IpAddr => uuid_ip::validate_ip(input),
        FormatId::Base64 => binary::validate_base64(input, Base64Policy {
            url_safe: false,
            padding: options.base64_padding,
        }).map(|_| ()),
        FormatId::Base64Url => binary::validate_base64(input, Base64Policy {
            url_safe: true,
            padding: options.base64_padding,
        }).map(|_| ()),
        FormatId::Hex => binary::validate_hex(input, options.hex).map(|_| ()),
        FormatId::NanoId => identity::validate_nanoid(input, options.nanoid_len),
        FormatId::BlockchainAddress => identity::validate_address(input),
        FormatId::BlockchainTransactionHash => identity::validate_transaction_hash(input),
    }
}

/// Resolves a canonical name and validates its decoded string bytes.
///
/// Name errors take precedence over input errors. Format failures are mapped
/// through [`FormatFailure::into_error`] with offset zero and source `SchemaVm`.
/// Callers must attach the enclosing raw token's location themselves: a decoded
/// failure index cannot be added to a raw JSON offset when escapes are present.
pub fn validate_named(name: &str, input: &[u8], options: &FormatOptions) -> Result<()> {
    let id = lookup(name)?;
    validate(id, input, options).map_err(|failure| failure.into_error(0, ErrorSource::SchemaVm))
}
