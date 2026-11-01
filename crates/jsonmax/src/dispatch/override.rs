//! Explicit scanner overrides and allocation-free selection reports.
//!
//! Parsing is independent of hardware. Selection uses fresh effective
//! capabilities and the checked portable resolver, without process-wide state.

#![forbid(unsafe_code)]

use super::{
    choose, detect, portable, unavailable_backend, BackendId, BackendOverride,
    CpuCapabilities, Dispatch, DispatchPolicy, DispatchReport, Error, ErrorCode,
    ErrorSource, Result,
};

/// Parses one exact, lowercase override spelling without inspecting hardware.
///
/// Accepted values are `auto`, `scalar`, `neon`, `avx2`, `avx512`, and each
/// backend name prefixed with `prefer:`. Bare backend names require that backend.
/// Whitespace, case variations, and aliases are not accepted.
///
/// # Errors
///
/// Any other value returns `InvalidToken` from `Scanner` at offset zero. The
/// diagnostic is static and never includes bytes from the supplied value.
pub fn parse(value: &str) -> Result<BackendOverride> {
    match value {
        "auto" => Ok(BackendOverride::Auto),
        "scalar" => Ok(BackendOverride::Require(BackendId::Scalar)),
        "neon" => Ok(BackendOverride::Require(BackendId::Neon)),
        "avx2" => Ok(BackendOverride::Require(BackendId::Avx2)),
        "avx512" => Ok(BackendOverride::Require(BackendId::Avx512)),
        "prefer:scalar" => Ok(BackendOverride::Prefer(BackendId::Scalar)),
        "prefer:neon" => Ok(BackendOverride::Prefer(BackendId::Neon)),
        "prefer:avx2" => Ok(BackendOverride::Prefer(BackendId::Avx2)),
        "prefer:avx512" => Ok(BackendOverride::Prefer(BackendId::Avx512)),
        _ => Err(Error::new(
            ErrorCode::InvalidToken,
            ErrorSource::Scanner,
            0,
            "invalid scanner backend override",
        )),
    }
}

/// Resolves an override and reports the request, policy, and effective selection.
///
/// `input_len` is a whole-input hint and can exceed the scanner's 64-byte block
/// limit. Supported explicit requests bypass all size thresholds and the
/// `allow_avx512` policy flag, while retaining the resolver's safety checks.
/// An unavailable preferred backend falls back to automatic policy selection.
///
/// If native resolution fails after the initial capability snapshot, automatic
/// and preferred requests resolve scalar and refresh the reported capabilities.
/// `used_fallback` is true only when a preferred backend could not be resolved;
/// automatic selection always reports it as false, including scalar recovery.
/// The report's selected backend describes the dispatch, not the tag of a later
/// block classification (an empty block can carry a Scalar tag).
///
/// # Errors
///
/// An unavailable required backend returns `ConstraintViolation` from `Scanner`
/// at offset zero. A required backend's resolution error is returned unchanged.
pub fn select(
    requested: BackendOverride,
    input_len: usize,
    policy: DispatchPolicy,
) -> Result<(Dispatch, DispatchReport)> {
    select_with(requested, input_len, policy, detect::detect, portable::resolve)
}

// Dependency injection is private so public callers cannot use synthetic
// capabilities to authorize native execution. It also lets tests exercise a
// change in availability without mutable globals or architecture assumptions.
fn select_with(
    requested: BackendOverride,
    input_len: usize,
    policy: DispatchPolicy,
    mut capabilities_now: impl FnMut() -> CpuCapabilities,
    mut resolve: impl FnMut(BackendId) -> Result<Dispatch>,
) -> Result<(Dispatch, DispatchReport)> {
    let mut capabilities = capabilities_now();
    let (candidate, mut used_fallback) = match requested {
        BackendOverride::Auto => (choose::choose(input_len, capabilities, policy), false),
        BackendOverride::Require(backend) => {
            if !capabilities.supports(backend) {
                return Err(unavailable_backend());
            }
            (backend, false)
        }
        BackendOverride::Prefer(backend) => {
            if capabilities.supports(backend) {
                (backend, false)
            } else {
                (choose::choose(input_len, capabilities, policy), true)
            }
        }
    };

    let dispatch = match resolve(candidate) {
        Ok(dispatch) => dispatch,
        Err(error) => {
            if candidate == BackendId::Scalar
                || matches!(requested, BackendOverride::Require(_))
            {
                return Err(error);
            }
            // Do not retry another native candidate using the stale snapshot.
            // Scalar resolution cannot fail in the production resolver.
            capabilities = capabilities_now();
            used_fallback = matches!(requested, BackendOverride::Prefer(_));
            resolve(BackendId::Scalar)?
        }
    };

    let report = DispatchReport {
        requested,
        selected: dispatch.backend(),
        capabilities,
        policy,
        input_len,
        used_fallback,
    };
    Ok((dispatch, report))
}
