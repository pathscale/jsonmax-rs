//! Compiled generated enum, Unit, and zero-field decoder parity with the VM.
use jmax_abi::number::NumericPolicy;
use jmax_native::{decode_into, NativeContext, NativeDecode, Reader, VmLimits};
use jmax_native_fixtures::{enum_unit, enum_unit_null};

fn agreement<T: NativeDecode + Default>(raw: &[u8], accepted: bool) {
    let program = T::build_program().unwrap();
    let limits = VmLimits::default();
    let numeric = NumericPolicy::strict_default();
    let vm = jmax_schema_vm::validate_borrowed(&program, raw, limits, numeric).map(|_| ());
    let mut ctx = NativeContext::with_program(&program, limits, numeric).unwrap();
    let native = decode_into(&mut ctx, raw, &mut T::default());
    assert_eq!(vm.is_ok(), accepted, "VM: {raw:?}: {vm:?}");
    assert_eq!(native.is_ok(), accepted, "native: {raw:?}: {native:?}");
    if let (Err(a), Err(b)) = (vm, native) {
        assert_eq!((a.code, a.byte_offset), (b.code, b.byte_offset), "{raw:?}");
    }
    // Exercise emitted code directly as well as the diagnosed public wrapper.
    let mut core_ctx = NativeContext::new(limits, numeric);
    let mut reader = Reader::new(raw);
    let core = T::decode_core(&mut reader, &mut core_ctx, &mut T::default()).and_then(|_| reader.end());
    assert_eq!(core.is_ok(), accepted, "core: {raw:?}");
}

#[test]
fn generated_enum_roots_accept_names_and_discriminants() {
    use enum_unit::models::Side;
    for raw in [br#""Bid""#.as_slice(), br#""\u0041sk""#.as_slice(), b"-2", b"7"] {
        agreement::<Side>(raw, true);
    }
    for raw in [br#""bid""#.as_slice(), br#""7""#.as_slice(), b"0", b"7.0", b"7e0", b"null", b"true", b"2147483648", b"-2147483649", b"[]", b"{}"] {
        agreement::<Side>(raw, false);
    }
    let mut ctx = NativeContext::new(Default::default(), NumericPolicy::strict_default());
    let mut side = Side::default();
    decode_into(&mut ctx, br#""Ask""#, &mut side).unwrap();
    assert!(matches!(side, Side::Ask));
    decode_into(&mut ctx, b"-2", &mut side).unwrap();
    assert!(matches!(side, Side::Bid));
}

#[test]
fn generated_unit_arrays_optional_values_and_empty_objects() {
    use enum_unit::models::{Empty, Envelope};
    let base = r#"{"side":"Ask","unit":null,"empty":{},"sides":["Bid",7],"units":[null,null]}"#;
    agreement::<Envelope>(base.as_bytes(), true);
    agreement::<Envelope>(base.replace("\"units\":", "\"maybe_side\":-2,\"maybe_unit\":null,\"units\":").as_bytes(), true);
    for (from, to) in [
        ("\"side\":\"Ask\"", "\"side\":\"Unknown\""),
        ("\"side\":\"Ask\"", "\"side\":7.0"),
        ("\"unit\":null", "\"unit\":{}"),
        ("\"empty\":{}", "\"empty\":null"),
        ("\"empty\":{}", "\"empty\":{\"x\":1}"),
        ("[\"Bid\",7]", "[\"Bid\",0]"),
        ("[null,null]", "[null,0]"),
        ("\"side\":\"Ask\"", "\"side\":\"Ask\",\"side\":7"),
    ] {
        agreement::<Envelope>(base.replace(from, to).as_bytes(), false);
    }
    agreement::<Envelope>(base.replace("\"units\":", "\"maybe_side\":null,\"units\":").as_bytes(), false);
    agreement::<enum_unit_null::models::Envelope>(base.replace("\"units\":", "\"maybe_side\":null,\"maybe_unit\":null,\"units\":").as_bytes(), true);
    for raw in [b"{}".as_slice(), b" \n { } \t"] { agreement::<Empty>(raw, true); }
    for raw in [b"null".as_slice(), b"[]", br#"{"x":null}"#, b"{} false"] { agreement::<Empty>(raw, false); }
}
