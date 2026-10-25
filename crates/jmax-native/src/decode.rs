//! The decision flow between a native decoder and the schema VM.

use crate::{
    Error, ErrorCode, ErrorSource, NativeContext, NativeResult, Reader, Result, ValidatedProgram,
};
use core::any::TypeId;
use jmax_schema_vm::ValidationScratch;

pub trait NativeDecode: Sized + 'static {
    /// True only when every enum membership check calls `record_enum_value`.
    /// Older/custom decoders use a conservative input-length upper bound.
    const COUNTS_ENUM_VALUES: bool = false;

    /// True when every unknown key uses Reader::unknown_key and all skipped
    /// values use Reader::skip_value over disjoint, forward-only input ranges.
    /// Every remaining byte must be strictly decoded against the reference
    /// schema. Legacy/custom decoders retain the conservative default.
    const COUNTS_NATIVE_BUDGET: bool = false;

    /// Build the reference program embedded next to the decoder. Called once
    /// per context; its error is returned by `decode_into` unchanged.
    fn build_program() -> Result<ValidatedProgram>;

    /// Decode the root value. Must decide exactly like the VM; see the contract.
    fn decode_core(
        reader: &mut Reader<'_>,
        ctx: &mut NativeContext,
        out: &mut Self,
    ) -> NativeResult<()>;

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

fn core_decode<T: NativeDecode>(
    ctx: &mut NativeContext,
    input: &[u8],
    out: &mut T,
) -> NativeResult<()> {
    let _t = jmax_trace::span!(NATIVE_SETUP);
    // A failed earlier decode may have left keys of objects it never closed.
    ctx.unknown.clear();
    ctx.decimals = 0;
    ctx.skipped_fields = 0;
    ctx.enum_values = 0;
    ctx.unknown_key_bytes = 0;
    ctx.unknown_fields = 0;
    ctx.skipped_bytes = 0;
    ctx.skip.new_input();
    let mut reader = Reader::new_shared_skip(input);
    T::decode_core(&mut reader, ctx, out)?;
    reader.end()?;
    Ok(())
}

/// Decode `input` into `out`. On error the contents of `out` are unspecified.
pub fn decode_into<T: NativeDecode>(
    ctx: &mut NativeContext,
    input: &[u8],
    out: &mut T,
) -> Result<()> {
    let _t = jmax_trace::span!(NATIVE_DECODE);
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
    // The native decoder runs first. Its answer stands on its own when the VM
    // provably could not have failed on resources: by the length alone, or, for
    // a long input, by the length together with the number of decimals the
    // decoder actually read (the VM bills each decimal a fixed amount). Otherwise
    // the VM decides, and a native Fail is always the VM's to explain.
    let native = core_decode(ctx, input, out);
    let by_length = ctx.safe_max_len.is_some_and(|max| input.len() <= max);
    let proven = native.is_ok()
        && (by_length
            || if T::COUNTS_NATIVE_BUDGET {
                jmax_schema_vm::budget::budget_safe_native_progress(
                    program,
                    input.len(),
                    ctx.decimals,
                    ctx.skipped_fields,
                    if T::COUNTS_ENUM_VALUES {
                        ctx.enum_values
                    } else {
                        input.len() as u64
                    },
                    ctx.unknown_fields,
                    ctx.unknown_key_bytes,
                    ctx.skipped_bytes,
                    ctx.limits,
                )
            } else {
                jmax_schema_vm::budget::budget_safe_native_counted(
                    program,
                    input.len(),
                    ctx.decimals,
                    ctx.skipped_fields,
                    if T::COUNTS_ENUM_VALUES {
                        ctx.enum_values
                    } else {
                        input.len() as u64
                    },
                    ctx.limits,
                )
            });
    if !proven {
        run_vm(program, ctx, input)?;
        if native.is_err() {
            return Err(diverged());
        }
    }
    out.post_check()
}

#[cfg(test)]
mod observed_tests {
    use super::*;
    use alloc::{format, string::String, vec};
    use jmax_schema_ir::{FieldId, ObjectShape, Property, ScalarConstraints, Schema, SchemaId, TypeId as SchemaTypeId, TypeNode, ValueKind};

    #[derive(Default)]
    struct OpenNative(u64);
    impl NativeDecode for OpenNative {
        const COUNTS_NATIVE_BUDGET: bool = true;
        const COUNTS_ENUM_VALUES: bool = true;
        fn build_program() -> Result<ValidatedProgram> {
            jmax_schema_aot::compile_graph(&Schema {
                name: "observed_native".into(), schema_id: SchemaId(7_049), root: SchemaTypeId(0),
                definitions: vec![], nodes: vec![
                    TypeNode::Object(ObjectShape {
                        nullable: false, reject_unknown: false, min_properties: None,
                        max_properties: None, properties: vec![Property {
                            field_id: FieldId(1), name: "n".into(), ty: SchemaTypeId(1), required: true, slot: None,
                        }],
                    }),
                    TypeNode::Scalar(ScalarConstraints::new(ValueKind::Integer)),
                ],
            })
        }
        fn decode_core(r: &mut Reader<'_>, ctx: &mut NativeContext, out: &mut Self) -> NativeResult<()> {
            r.expect(b'{')?;
            let mark = ctx.begin_object();
            let mut seen = false;
            if !r.eat(b'}') {
                loop {
                    if r.match_key_decoded(ctx, b"\"n\":") {
                        if seen && ctx.limits().syntax.reject_duplicate_keys { return Err(crate::Fail); }
                        seen = true;
                        out.0 = r.uint(ctx, u64::MAX)?;
                    } else {
                        r.unknown_key(ctx)?;
                        r.skip_value(ctx, 1)?;
                    }
                    if r.eat(b',') { continue; }
                    r.expect(b'}')?;
                    break;
                }
            }
            if !seen { return Err(crate::Fail); }
            ctx.end_object(mark)
        }
    }

    #[test]
    fn actual_reader_counts_prove_large_skip_without_another_input_walk() -> Result<()> {
        let program = OpenNative::build_program()?;
        let raw = format!("{{\"n\":7,\"payload\":\"{}\"}}", String::from_utf8(vec![b'x'; 1_000_000]).unwrap());
        let mut limits = crate::VmLimits::default();
        limits.syntax.reject_unknown_keys = false;
        limits.max_scratch_bytes = 16 * 1024 * 1024;
        let mut ctx = NativeContext::with_program(&program, limits, crate::NumericPolicy::strict_default())?;
        core_decode(&mut ctx, raw.as_bytes(), &mut OpenNative::default()).unwrap();
        assert_eq!(ctx.skipped_bytes, 1_000_002);
        assert!(jmax_schema_vm::budget::budget_safe_native_progress(
            &program, raw.len(), ctx.decimals, ctx.skipped_fields, ctx.enum_values,
            ctx.unknown_fields, ctx.unknown_key_bytes, ctx.skipped_bytes, limits,
        ));
        run_vm(&program, &ctx, raw.as_bytes())?;
        Ok(())
    }

    #[test]
    fn observed_native_and_vm_agree_at_resource_boundaries() -> Result<()> {
        let program = OpenNative::build_program()?;
        for raw in [br#"{"n":7,"x":{"a":[1,2,{"b":"c\nd"}]},"other":true}"#.as_slice(),
            br#"{"n":7,"x":false,"\u0078":true}"#,
            br#"{"n":7,"x":[1,]}"#] {
            for steps in [1, raw.len() as u64, 512, 4096, 1_000_000] {
                for scratch in [0, 256, 4096, 1_000_000] {
                    let mut limits = crate::VmLimits::default();
                    limits.syntax.reject_unknown_keys = false;
                    limits.max_steps = steps;
                    limits.max_scratch_bytes = scratch;
                    let mut ctx = NativeContext::with_program(&program, limits, crate::NumericPolicy::strict_default())?;
                    let expected = run_vm(&program, &ctx, raw).err().map(|e| (e.code, e.source, e.byte_offset));
                    let actual = decode_into(&mut ctx, raw, &mut OpenNative::default()).err().map(|e| (e.code, e.source, e.byte_offset));
                    assert_eq!(actual, expected, "steps={steps} scratch={scratch} raw={raw:?}");
                }
            }
        }
        Ok(())
    }
}
