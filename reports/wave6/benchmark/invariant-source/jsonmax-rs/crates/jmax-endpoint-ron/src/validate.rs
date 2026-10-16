//! Semantic validation of normalized endpoint configurations.
//!
//! The endpoint-model registry owns the canonical invariants. Keeping this
//! adapter on that path gives loaded configurations the same reference, method,
//! role, header and error semantics as models constructed by other adapters.

use crate::{EndpointModel, Result};

/// Validate every record in a normalized model, including unused declarations.
///
/// Registry construction checks names, global endpoint codes, canonical methods,
/// reference kinds and depth, nominal recursion, roles, headers and error keys.
/// Its canonical ABI errors are returned unchanged, including diagnostic context
/// and deterministic declaration-order failures. The temporary borrowed index
/// is discarded on success; the caller's model is never modified.
///
/// This validates model semantics only. Version requirements are checked by
/// `crate::version::validate_version`, and no payload validation proof is issued.
pub fn validate_model(model: &EndpointModel) -> Result<()> {
    jmax_endpoint_model::registry::build_registry(model).map(|_| ())
}
