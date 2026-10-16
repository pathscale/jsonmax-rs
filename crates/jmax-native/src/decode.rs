//! The decision flow between a native decoder and the schema VM.

use crate::{
    Error, ErrorCode, ErrorSource, NativeContext, NativeResult, Reader, Result, ValidatedProgram,
};
use core::any::TypeId;
use jmax_schema_vm::ValidationScratch;

pub trait NativeDecode: Sized + 'static {
    /// Build the reference program embedded next to the decoder. Called once
    /// per context; its error is returned by `decode_into` unchanged.
    fn build_program() -> Result<ValidatedProgram>;

    /// Decode the root value. Must decide exactly like the VM; see the contract.
    fn decode_core(reader: &mut Reader<'_>, ctx: &mut NativeContext, out: &mut Self) -> NativeResult<()>;

    /// Value checks the schema cannot express. Runs on both routes.
    fn post_check(&self) -> Result<()> {
        Ok(())
    }
}

fn diverged() -> Error {
    Error::new(
        ErrorCode::InternalError,
        ErrorSource::Decode,
        0,
        "native decoder diverged from schema VM",
    )
}

fn run_vm(program: &ValidatedProgram, ctx: &NativeContext, input: &[u8]) -> Result<()> {
    // A fresh scratch: retained capacity would otherwise count against the
    // scratch budget and change the VM's answer.
    jmax_schema_vm::direct::validate_borrowed_into(
        program,
        input,
        ctx.limits,
        ctx.requested,
        &mut ValidationScratch::default(),
    )
    .map(|_| ())
}

fn core_decode<T: NativeDecode>(ctx: &mut NativeContext, input: &[u8], out: &mut T) -> NativeResult<()> {
    // A failed earlier decode may have left keys of objects it never closed.
    ctx.unknown.clear();
    let mut reader = Reader::new(input);
    T::decode_core(&mut reader, ctx, out)?;
    reader.end()
}

/// Decode `input` into `out`. On error the contents of `out` are unspecified.
pub fn decode_into<T: NativeDecode>(ctx: &mut NativeContext, input: &[u8], out: &mut T) -> Result<()> {
    if ctx.program_type != Some(TypeId::of::<T>()) {
        let program = T::build_program()?;
        ctx.install(program, TypeId::of::<T>())?;
    }
    let program = ctx.program.take().ok_or_else(diverged)?;
    let result = decide(&program, ctx, input, out);
    ctx.program = Some(program);
    result
}

fn decide<T: NativeDecode>(
    program: &ValidatedProgram,
    ctx: &mut NativeContext,
    input: &[u8],
    out: &mut T,
) -> Result<()> {
    let safe = ctx.safe_max_len.is_some_and(|max| input.len() <= max);
    if !safe {
        run_vm(program, ctx, input)?;
        core_decode(ctx, input, out).map_err(|_| diverged())?;
    } else if core_decode(ctx, input, out).is_err() {
        return match run_vm(program, ctx, input) {
            Err(error) => Err(error),
            Ok(()) => Err(diverged()),
        };
    }
    out.post_check()
}
