//! JSON parser which picks up values directly without performing tokenization
extern crate core;
extern crate fnv;
// Exactly one compatibility backend is selected. `portable` overrides every ISA.
#[cfg(all(target_arch = "x86_64", not(feature = "portable")))]
#[path = "avx.rs"]
mod x86;
#[cfg(any(feature = "portable", not(any(target_arch = "aarch64", target_arch = "x86_64"))))]
mod emulated;
#[cfg(all(target_arch = "aarch64", not(feature = "portable")))]
mod neon;
mod bit;
mod error;
#[doc(hidden)]
pub mod index_builder;
#[doc(hidden)]
pub mod parser;
mod pikkr;
#[doc(hidden)]
pub mod query;
mod result;
mod utf8;

#[cfg(any(feature = "portable", not(any(target_arch = "aarch64", target_arch = "x86_64"))))]
pub use emulated::avx;
#[cfg(all(target_arch = "aarch64", not(feature = "portable")))]
pub use neon::avx;
#[cfg(all(target_arch = "x86_64", not(feature = "portable")))]
pub use x86::avx;

pub use error::{Error, ErrorKind};
pub use pikkr::Pikkr;
pub use result::Result;
