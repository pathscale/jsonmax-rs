#![no_main]
//! Generated native decoders against the schema VM and the value oracle.
//!
//! byte 0 selects the decoder, byte 1 the limit set, the rest is the input; see
//! `jmax_native_fixtures::fuzzing`. A panic is a finding.
//!
//!     cd crates/jmax-native-fixtures/fuzz
//!     mkdir -p corpus/native_vs_vm
//!     cargo run -p jmax-native-fixtures --example fuzz_seeds -- corpus/native_vs_vm
//!     cargo +nightly fuzz run native_vs_vm
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    jmax_native_fixtures::fuzzing::native_vs_vm(data);
});
