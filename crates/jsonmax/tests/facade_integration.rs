extern crate alloc;

use std::sync::Arc;

use jmax_abi::{schema::SchemaFieldIr, syntax::ScalarKind};
use jsonmax::{
    ByteSink, CompileLimits, DomScratch, EncodeOptions, Error, ErrorCode, ErrorSource, FieldId,
    JsonDecode, JsonEncode, ParseOptions, Payload, QueryMode, QueryOptions, Result, SchemaId,
    SchemaIr, SchemaProvider, Scratch, Selection, SlotValue, ValidatedSlots, encode, endpoint,
    parse, query,
};

type TestResult = Result<()>;

fn schema_ir() -> SchemaIr {
    let field = |id, name: &str, kind, required| SchemaFieldIr {
        field_id: FieldId(id),
        name: name.into(),
        required,
        scalar_kind: kind,
        format: None,
        min_len: None,
        max_len: None,
        min_i64: None,
        max_i64: None,
    };
    let mut number = field(1, "wanted", Some(ScalarKind::Number), true);
    number.min_i64 = Some(0);
    number.max_i64 = Some(10);
    SchemaIr {
        name: "facade-integration".into(),
        schema_id: SchemaId(66),
        required_count: 1,
        reject_unknown: true,
        fields: vec![
            number,
            field(2, "text", Some(ScalarKind::String), false),
            field(3, "free", None, false),
        ],
    }
}
fn schema() -> Result<endpoint::CompiledSchema> {
    endpoint::compile_schema(&schema_ir(), CompileLimits::default_limits())
}
fn selection(strict: bool) -> Selection {
    Selection {
        field_ids: vec![FieldId(1)],
        strict_validate_unselected: strict,
    }
}
fn query_options(mode: QueryMode) -> QueryOptions {
    QueryOptions {
        mode,
        ..QueryOptions::default()
    }
}

#[test]
fn native_owned_borrowed_and_explicit_legacy_encoding_agree() -> TestResult {
    let raw = " {\"text\":\"é😀\",\"escaped\":\"h\\u00e9\",\"n\":1.250e+2,\"a\":[true,null,{}]} ";
    let options = ParseOptions::default();
    let borrowed = parse::parse_borrowed(raw.as_bytes(), options)?;
    assert_eq!(borrowed.raw().as_ptr(), raw.as_ptr());
    assert_eq!(borrowed.decoded(), "hé".as_bytes());
    let owned = parse::parse_slice(raw.as_bytes(), options)?;
    assert_ne!(owned.raw().as_ptr(), raw.as_ptr());
    assert_eq!(owned.node_count(), borrowed.node_count());
    let bytes = encode::encode_document(&owned, EncodeOptions::default())?;
    assert_eq!(
        bytes,
        encode::encode_borrowed_document(&borrowed, EncodeOptions::default())?
    );
    assert!(
        std::str::from_utf8(&bytes)
            .unwrap_or("")
            .contains("1.250e+2")
    );
    let legacy = parse::to_legacy_document(&owned)?;
    assert!(legacy.payload.shares_backing_with(owned.payload()));
    let rebuilt = parse::from_legacy_document(legacy, options)?;
    assert!(rebuilt.payload().shares_backing_with(owned.payload()));
    assert_eq!(
        bytes,
        encode::document_to_vec(&rebuilt, EncodeOptions::default())?
    );
    Ok(())
}

#[test]
fn vec_arc_and_payload_subviews_preserve_the_actual_owner() -> TestResult {
    let options = ParseOptions::default();
    let raw = br#"{"x":7}"#.to_vec();
    let pointer = raw.as_ptr();
    let document = parse::parse_owned(raw, options)?;
    assert_eq!(document.raw().as_ptr(), pointer);
    let owner = Arc::new(br#"{"x":7}"#.to_vec());
    let pointer = owner.as_ptr();
    let document = parse::parse_arc_owner(owner.clone(), options)?;
    assert_eq!(document.raw().as_ptr(), pointer);
    assert_eq!(Arc::strong_count(&owner), 2);
    drop(document);
    assert_eq!(Arc::strong_count(&owner), 1);
    let backing = Payload::from_vec(b"prefix[1,2]suffix".to_vec());
    let view = backing.slice(6..11)?;
    let pointer = view.as_slice().as_ptr();
    let document = parse::parse_payload(view, options)?;
    assert!(document.payload().shares_backing_with(&backing));
    assert_eq!(document.raw().as_ptr(), pointer);
    assert_eq!(document.raw(), b"[1,2]");
    Ok(())
}

#[test]
fn native_scratch_recycling_and_error_recovery() -> TestResult {
    let mut scratch = DomScratch::new();
    let options = ParseOptions::default();
    let raw = br#"{"a":["\n",1,2,3]}"#;
    let document = parse::parse_borrowed_with_scratch(raw, options, &mut scratch)?;
    let words = document.tape().len();
    document.recycle(&mut scratch);
    assert!(scratch.tape_capacity() >= words);
    assert!(parse::parse_borrowed_with_scratch(b"[1,]", options, &mut scratch).is_err());
    let payload = Payload::from_slice(raw);
    let document = parse::parse_payload_with_scratch(payload.clone(), options, &mut scratch)?;
    assert!(document.payload().shares_backing_with(&payload));
    document.recycle(&mut scratch);
    let document = parse::parse_payload_into(&payload, options, &mut scratch)?;
    assert_eq!(document.raw(), raw);
    Ok(())
}

#[test]
fn legacy_conversion_rejects_mutated_layout_and_rechecks_numeric_policy() -> TestResult {
    let options = ParseOptions::default();
    let doc = parse::parse_slice(br#"{"a":1}"#, options)?;
    let mut legacy = parse::to_legacy_document(&doc)?;
    legacy.parts.spans.clear();
    assert!(parse::from_legacy_document(legacy, options).is_err());
    let mut relaxed = options;
    relaxed.numeric.reject_negative_zero = false;
    let doc = parse::parse_slice(b"-0", relaxed)?;
    assert!(parse::from_legacy_document(parse::to_legacy_document(&doc)?, options).is_err());
    Ok(())
}

#[test]
fn borrowed_syntax_proof_checks_discarded_values_and_tail() -> TestResult {
    let mut scratch = Scratch::default();
    let options = ParseOptions::default();
    let proof = parse::validate_borrowed(br#"{"x":[1,"\n"]}"#, options, &mut scratch)?;
    assert_eq!(proof.schema_fingerprint(), 0);
    for raw in [
        br#"{"x":[1,]}"#.as_slice(),
        br#"{"x":{"a":1,"\u0061":2}}"#,
        br#"{"x":"\uD800"}"#,
        b"{} false",
        b"[-0]",
        b"[1e999]",
    ] {
        assert!(
            parse::validate_borrowed(raw, options, &mut scratch).is_err(),
            "{raw:?}"
        );
    }
    parse::validate_borrowed(b"true", options, &mut scratch)?;
    Ok(())
}

#[test]
fn schema_proof_only_validation_never_publishes_slot_output() -> TestResult {
    let schema = schema()?;
    let raw = br#"{"wanted":7,"free":[1,2]}"#;
    let options = ParseOptions::default();
    let payload = Payload::from_slice(raw);
    let mut scratch = endpoint::ValidationScratch::default();
    let proof = endpoint::validate_payload(payload.clone(), &schema, options)?;
    assert_eq!(proof.schema_fingerprint(), schema.fingerprint());
    endpoint::validate_payload_with_scratch(payload.clone(), &schema, options, &mut scratch)?;
    assert!(scratch.slots().is_none());
    endpoint::validate_borrowed(&schema, raw, options, &mut scratch)?;
    assert!(scratch.slots().is_none());
    let slots = endpoint::extract_payload_with_scratch(
        &payload,
        &schema,
        &selection(true),
        options,
        &mut scratch,
    )?;
    assert_eq!(slots.slots, vec![SlotValue::U64(7)]);
    assert!(slots.payload.shares_backing_with(&payload));
    endpoint::validate_payload_with_scratch(payload, &schema, options, &mut scratch)?;
    assert!(scratch.slots().is_none());
    Ok(())
}

#[test]
fn schema_borrowed_extraction_keeps_input_and_scratch_lifetimes_distinct() -> TestResult {
    let schema = schema()?;
    let raw = br#"{"wanted":7,"text":"h\u00e9"}"#;
    let options = ParseOptions::default();
    let selected = Selection {
        field_ids: vec![FieldId(2), FieldId(1)],
        strict_validate_unselected: true,
    };
    let values = endpoint::extract_borrowed(&schema, &selected, raw, options)?;
    assert_eq!(values.raw.as_ptr(), raw.as_ptr());
    assert_eq!(values.slots[0], SlotValue::U64(7));
    assert!(matches!(values.slots[1], SlotValue::Str(_)));
    assert_eq!(values.decoded, "hé".as_bytes());
    let mut scratch = endpoint::ValidationScratch::default();
    {
        let values = endpoint::extract_borrowed_with_scratch(
            &schema,
            &selected,
            raw,
            options,
            &mut scratch,
        )?;
        assert_eq!(values.raw.as_ptr(), raw.as_ptr());
        assert_eq!(values.slots[0], SlotValue::U64(7));
    }
    assert!(
        endpoint::extract_borrowed_with_scratch(
            &schema,
            &selected,
            br#"{"wanted":11}"#,
            options,
            &mut scratch
        )
        .is_err()
    );
    let owned = endpoint::extract_payload(Payload::from_slice(raw), &schema, &selected, options)?;
    assert!(matches!(owned.slots[0], SlotValue::Str(_)));
    assert_eq!(owned.slots[1], SlotValue::U64(7));
    Ok(())
}

#[test]
fn schema_query_results_skip_unselected_interiors_without_proofs() -> TestResult {
    let schema = schema()?;
    let raw = br#"{"wanted":7,"free":{"bad":truX}}"#;
    let options = ParseOptions::default();
    let selected = selection(true);
    assert!(endpoint::extract_borrowed(&schema, &selected, raw, options).is_err());
    let values = endpoint::extract_query_borrowed(&schema, &selected, raw, options)?;
    assert_eq!(values.raw.as_ptr(), raw.as_ptr());
    assert_eq!(values.slots[0], SlotValue::U64(7));
    let payload = Payload::from_slice(raw);
    let values = endpoint::extract_query_payload(payload.clone(), &schema, &selected, options)?;
    assert!(values.payload.shares_backing_with(&payload));
    assert_eq!(values.slots[0], SlotValue::U64(7));
    assert!(
        endpoint::extract_borrowed(&schema, &selection(false), br#"{"wanted":7}"#, options)
            .is_err()
    );
    Ok(())
}

#[test]
fn strict_schema_extraction_enforces_unselected_constraints() -> TestResult {
    let schema = schema()?;
    let mut scratch = endpoint::ValidationScratch::default();
    let selected = Selection {
        field_ids: vec![FieldId(2)],
        strict_validate_unselected: true,
    };
    for raw in [
        br#"{"wanted":11,"text":"ok"}"#.as_slice(),
        br#"{"text":"ok"}"#,
        br#"{"wanted":7,"free":[01],"text":"ok"}"#,
        br#"{"wanted":7,"free":{"a":1,"\u0061":2},"text":"ok"}"#,
    ] {
        assert!(
            endpoint::extract_borrowed(&schema, &selected, raw, ParseOptions::default()).is_err()
        );
        assert!(
            endpoint::validate_borrowed(&schema, raw, ParseOptions::default(), &mut scratch)
                .is_err()
        );
    }
    Ok(())
}

#[test]
fn path_queries_borrow_input_preserve_path_order_and_match_limits() -> TestResult {
    let raw = br#"{"list":[{"x":1},{"x":2}],"a.b":"ok"}"#;
    let query = query::compile_paths(&["list[1].x", "a\\.b", "list[1].x", "missing"])?;
    let result = query::execute_borrowed(&query, raw, QueryOptions::default())?;
    assert_eq!(result.raw().as_ptr(), raw.as_ptr());
    assert_eq!(result.raw_match(0, 0), Some(b"2".as_slice()));
    assert_eq!(result.raw_match(1, 0), Some(br#""ok""#.as_slice()));
    assert_eq!(result.raw_match(2, 0), Some(b"2".as_slice()));
    assert!(result.matches()[3].is_empty());
    assert!(result.fully_validated());
    let mut options = QueryOptions::default();
    options.max_matches = 2;
    assert!(query::execute_borrowed(&query, raw, options).is_err());
    let payload = Payload::from_slice(raw);
    let owned = query::query_payload(payload.clone(), &query, QueryOptions::default())?;
    assert!(owned.payload().shares_backing_with(&payload));
    assert_eq!(owned.matches(), result.matches());
    Ok(())
}

#[test]
fn query_only_path_skips_are_honest_but_selected_containers_are_strict() -> TestResult {
    let raw = br#"{"wanted":7,"free":{"bad":truX}}"#;
    let selected = query::compile_paths(&["wanted"])?;
    assert!(query::execute_borrowed(&selected, raw, query_options(QueryMode::Strict)).is_err());
    let result = query::execute_borrowed(&selected, raw, query_options(QueryMode::Query))?;
    assert_eq!(result.raw_match(0, 0), Some(b"7".as_slice()));
    assert!(!result.fully_validated());
    for paths in [vec!["free"], vec!["$", "wanted"], vec!["free", "free.bad"]] {
        let query = query::compile_paths(&paths)?;
        assert!(query::execute_borrowed(&query, raw, query_options(QueryMode::Query)).is_err());
    }
    let missing = query::compile_paths(&["missing"])?;
    let result = query::execute_borrowed(&missing, raw, query_options(QueryMode::Query))?;
    assert!(result.matches()[0].is_empty());
    assert!(!result.fully_validated());
    Ok(())
}

#[test]
fn strict_path_selection_checks_tail_duplicate_and_numeric_policies() -> TestResult {
    let query = query::compile_paths(&["wanted"])?;
    for raw in [
        br#"{"wanted":7} false"#.as_slice(),
        br#"{"wanted":7,"free":[-0]}"#,
        br#"{"wanted":7,"free":{"a":1,"\u0061":2}}"#,
    ] {
        assert!(query::execute_borrowed(&query, raw, QueryOptions::default()).is_err());
    }
    Ok(())
}

struct FailingEncode;
impl JsonEncode for FailingEncode {
    fn encode_json(&self, sink: &mut dyn ByteSink) -> Result<()> {
        sink.write_bytes(b"{")?;
        Err(Error::new(
            ErrorCode::ConstraintViolation,
            ErrorSource::Encode,
            0,
            "test encoding failure",
        ))
    }
}

#[test]
fn staged_and_streaming_failure_publication_remain_distinct() -> TestResult {
    let mut sink = b"prefix".to_vec();
    assert!(encode::to_sink(&FailingEncode, &mut sink, EncodeOptions::default()).is_err());
    assert_eq!(sink, b"prefix");
    assert!(
        encode::to_sink_streaming(&FailingEncode, &mut sink, EncodeOptions::default()).is_err()
    );
    assert_eq!(sink, b"prefix{");
    let document = parse::parse_slice(br#"{"x":1}"#, ParseOptions::default())?;
    let mut sink = b"prefix".to_vec();
    let options = EncodeOptions {
        max_output_bytes: 2,
    };
    assert!(encode::encode_document_to_sink(&document, &mut sink, options).is_err());
    assert_eq!(sink, b"prefix");
    assert!(encode::encode_document_streaming(&document, &mut sink, options).is_err());
    assert_eq!(sink, b"prefix{");
    Ok(())
}

#[test]
fn native_streaming_limits_exclude_sink_prefix_and_reuse_writer_scratch() -> TestResult {
    let raw = br#"{"x":"\n"}"#;
    let document = parse::parse_borrowed(raw, ParseOptions::default())?;
    let options = EncodeOptions {
        max_output_bytes: raw.len(),
    };
    let mut sink = vec![b'p'; 100];
    encode::encode_borrowed_document_streaming(&document, &mut sink, options)?;
    assert_eq!(&sink[100..], raw);
    let mut output = jsonmax::OutputBuffer::new(raw.len());
    let mut scratch = jmax_encode::WriterScratch::default();
    encode::borrowed_document_to_buffer(&document, &mut output, &mut scratch)?;
    assert_eq!(output.as_bytes(), raw);
    encode::borrowed_document_to_buffer(&document, &mut output, &mut scratch)?;
    assert_eq!(output.as_bytes(), raw);
    Ok(())
}

struct OwnedTyped(Payload);
impl SchemaProvider for OwnedTyped {
    fn schema_ir() -> SchemaIr {
        schema_ir()
    }
    fn schema_fingerprint() -> u64 {
        schema().map(|schema| schema.fingerprint()).unwrap_or(0)
    }
}
impl JsonDecode for OwnedTyped {
    fn decode_slots(slots: &ValidatedSlots) -> Result<Self> {
        if slots.slots.first() != Some(&SlotValue::U64(7)) {
            return Err(Error::new(
                ErrorCode::TypeMismatch,
                ErrorSource::Decode,
                0,
                "test slot mismatch",
            ));
        }
        Ok(Self(slots.payload.clone()))
    }
}

// This custom decoder explicitly chooses to own the input. That copy belongs
// to the decoder, rather than being an unconditional facade compatibility step.
impl jsonmax::JsonDecodeBorrowed for OwnedTyped {
    fn decode_borrowed(reader: jmax_schema_aot::BorrowedSlotReader<'_>) -> Result<Self> {
        if reader.slot(0, FieldId(1))? != SlotValue::U64(7) {
            return Err(Error::new(
                ErrorCode::TypeMismatch,
                ErrorSource::Decode,
                0,
                "test slot mismatch",
            ));
        }
        Ok(Self(Payload::from_slice(reader.values().raw)))
    }
}

#[test]
fn typed_borrowed_custom_decoder_can_choose_to_own_its_input() -> TestResult {
    let raw = br#"{"wanted":7}"#.to_vec();
    let value = parse::from_borrowed::<OwnedTyped>(&raw, ParseOptions::default())?;
    assert_eq!(value.0.as_slice(), raw);
    assert_ne!(value.0.as_slice().as_ptr(), raw.as_ptr());
    let value =
        parse::from_borrowed_with_schema::<OwnedTyped>(&raw, &schema()?, ParseOptions::default())?;
    drop(raw);
    assert_eq!(value.0.as_slice(), br#"{"wanted":7}"#);
    Ok(())
}

#[cfg(feature = "rusty-alloc")]
#[test]
fn optional_allocator_is_available_without_library_global_installation() {
    fn is_allocator<T: std::alloc::GlobalAlloc>() {}
    is_allocator::<jsonmax::RustyAlloc>();
}

#[test]
fn canonical_hosted_validator_carries_exact_owner_capability() -> TestResult {
    use jmax_nagoya::{CancellationToken, ParseValidator, worker::SchemaValidator};
    let schema = schema()?;
    let numeric = jmax_abi::number::NumericPolicy::strict_default();
    let validator = SchemaValidator::new(
        Arc::new(schema.program().clone()),
        selection(true),
        numeric,
        16_000_000,
    )?;
    let owner = Payload::from_arc_owner(Arc::new(br#" {"wanted":3,"text":"\u00e9"} "#.to_vec()))?;
    let payload = owner.slice(1..owner.len() - 1)?;
    let output = validator.validate_output(
        payload.clone(),
        ParseOptions::default().limits,
        &mut Scratch::default(),
        1 << 20,
        1 << 20,
        &CancellationToken::new(),
    )?;
    let capability = output.capability().unwrap();
    let values = output.slots();
    let vm_limits = jmax_schema_vm::VmLimits {
        syntax: ParseOptions::default().limits,
        max_steps: 16_000_000,
        max_scratch_bytes: 1 << 20,
    };
    let policy = jmax_schema_vm::direct::policy_fingerprint(vm_limits, numeric);
    assert!(capability.matches_payload(
        &payload,
        values.decoded.len(),
        schema.fingerprint(),
        policy
    ));
    assert!(values.payload.shares_backing_with(&payload));
    assert!(!capability.matches_payload(
        &owner,
        values.decoded.len(),
        schema.fingerprint(),
        policy
    ));
    assert!(!capability.matches_payload(
        &payload,
        values.decoded.len(),
        schema.fingerprint(),
        policy ^ 1
    ));
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert!(matches!(validator.validate_output(
        payload, ParseOptions::default().limits, &mut Scratch::default(),
        1 << 20, 1 << 20, &cancelled,
    ), Err(error) if error.code == ErrorCode::Cancelled));
    Ok(())
}

#[test]
fn payload_limit_is_inclusive_for_owned_borrowed_and_proof_only_paths() -> TestResult {
    let mut options = ParseOptions::default();
    options.limits.max_payload_bytes = 4;
    parse::parse_borrowed(b"null", options)?;
    parse::parse_owned(b"null".to_vec(), options)?;
    parse::validate_borrowed(b"null", options, &mut Scratch::default())?;
    options.limits.max_payload_bytes = 3;
    assert!(parse::parse_borrowed(b"null", options).is_err());
    assert!(parse::parse_owned(b"null".to_vec(), options).is_err());
    assert!(parse::validate_borrowed(b"null", options, &mut Scratch::default()).is_err());
    Ok(())
}

#[derive(Debug, PartialEq, jmax_derive::JsonDecode, jmax_derive::JsonSchema)]
struct DerivedBorrowed {
    wanted: u32,
    text: Option<String>,
}

#[test]
fn derived_borrowed_decode_uses_checked_reader_and_reusable_slots() -> TestResult {
    let raw = br#"{"wanted":7,"text":"h\u00e9"}"#;
    let schema = endpoint::compile_schema(
        &DerivedBorrowed::schema_ir(),
        CompileLimits::default_limits(),
    )?;
    let mut scratch = endpoint::ValidationScratch::default();
    for _ in 0..3 {
        let value = parse::from_borrowed_with_schema_into::<DerivedBorrowed>(
            raw,
            &schema,
            ParseOptions::default(),
            &mut scratch,
        )?;
        assert_eq!(
            value,
            DerivedBorrowed {
                wanted: 7,
                text: Some("hé".into())
            }
        );
    }
    assert_eq!(
        parse::from_borrowed::<DerivedBorrowed>(raw, ParseOptions::default())?.wanted,
        7
    );
    let owned = parse::from_slice::<DerivedBorrowed>(raw)?;
    assert_eq!(owned.value().text.as_deref(), Some("hé"));
    assert!(
        parse::from_borrowed_with_schema::<DerivedBorrowed>(
            raw,
            &self::schema()?,
            ParseOptions::default()
        )
        .is_err()
    );
    Ok(())
}

struct PointerTyped(usize);
impl SchemaProvider for PointerTyped {
    fn schema_ir() -> SchemaIr {
        schema_ir()
    }
    fn schema_fingerprint() -> u64 {
        OwnedTyped::schema_fingerprint()
    }
}
impl jsonmax::JsonDecodeBorrowed for PointerTyped {
    fn decode_borrowed(reader: jmax_schema_aot::BorrowedSlotReader<'_>) -> Result<Self> {
        assert_eq!(reader.slot(0, FieldId(1))?, SlotValue::U64(7));
        Ok(Self(reader.values().raw.as_ptr() as usize))
    }
}

#[test]
fn typed_borrowed_decoder_observes_original_payload_pointer() -> TestResult {
    let raw = br#"{"wanted":7}"#.to_vec();
    let value = parse::from_borrowed::<PointerTyped>(&raw, ParseOptions::default())?;
    assert_eq!(value.0, raw.as_ptr() as usize);
    let mut scratch = endpoint::ValidationScratch::default();
    let value = parse::from_borrowed_with_schema_into::<PointerTyped>(
        &raw,
        &schema()?,
        ParseOptions::default(),
        &mut scratch,
    )?;
    assert_eq!(value.0, raw.as_ptr() as usize);
    Ok(())
}

#[test]
fn precompiled_queries_recycle_storage_and_preserve_sparse_semantics() -> TestResult {
    let query = query::compile_paths(&["list[1].a\\.b", "list[1].a\\.b", "missing"])?;
    let mut scratch = query::QueryScratch::default();
    let raw = br#"{"list":[{"junk":truX},{"a.b":"ok"}],"free":{"bad":truX}}"#;
    for _ in 0..3 {
        let values = query::execute_borrowed_with_scratch(
            &query,
            raw,
            query_options(QueryMode::Query),
            &mut scratch,
        )?;
        assert_eq!(values.raw_match(0, 0), Some(br#""ok""#.as_slice()));
        assert_eq!(values.matches()[0], values.matches()[1]);
        assert!(values.matches()[2].is_empty());
        assert!(!values.fully_validated());
        scratch.recycle_borrowed(values);
    }
    for bad in [
        br#"{"list":[],}"#.as_slice(),
        br#"{"list":[0,]}"#,
        br#"{"list":[],"\u006cist":[]}"#,
        br#"{"list":[]} false"#,
    ] {
        assert!(
            query::execute_borrowed_with_scratch(
                &query,
                bad,
                query_options(QueryMode::Query),
                &mut scratch
            )
            .is_err()
        );
    }
    let valid = br#"{"list":[0,{"a.b":7}]}"#;
    for mode in [QueryMode::Strict, QueryMode::Query] {
        let values = query::execute_with_scratch(
            &query,
            Payload::from_slice(valid),
            query_options(mode),
            &mut scratch,
        )?;
        assert_eq!(values.raw_match(0, 0), Some(b"7".as_slice()));
        scratch.recycle_owned(values);
    }
    Ok(())
}
