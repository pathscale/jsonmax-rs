//! Generated native decoders for the Binance fixtures, checked in so that they
//! compile with the workspace, and the harness that holds them to the VM.
//!
//! `binance` is generated from `fixtures/native/binance.ron` with the strict
//! specification; `binance_open` from the same model with a specification that
//! accepts unknown keys, which exercises the skip path.

pub mod fuzzing;
pub mod harness;
pub mod regen;

/// Whether the checked-in modules were generated with layout prediction.
pub const PREDICT_LAYOUT: bool = true;

#[allow(clippy::all, dead_code, unused)]
pub mod binance;
#[allow(clippy::all, dead_code, unused)]
pub mod binance_open;
#[allow(clippy::all, dead_code, unused)]
pub mod binance_plain;
#[allow(clippy::all, dead_code, unused)]
pub mod shapes;
#[allow(clippy::all, dead_code, unused)]
pub mod shapes_open;
