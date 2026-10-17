//! Stage 5 supplemental harness: operation-only timings and separate allocation passes.
use jmax_bench::{CorpusConfig, Payload, Workload};
use jmax_schema_ir::{
    ObjectShape, Property, ScalarConstraints, Schema, TypeId, TypeNode, ValueKind,
};
use std::{
    fs::File,
    hint::black_box,
    io::{BufWriter, Write},
    sync::{Arc, atomic::Ordering},
    time::Instant,
};
type R<T> = Result<T, Box<dyn std::error::Error>>;
fn counters() -> (usize, usize) {
    (
        super::ALLOC_CALLS.load(Ordering::Relaxed),
        super::ALLOC_BYTES.load(Ordering::Relaxed),
    )
}
fn bench(
    out: &mut impl Write,
    w: &Workload,
    mode: &str,
    regime: &str,
    mut f: impl FnMut(usize) -> R<()>,
) -> R<()> {
    let n = w.payloads.len();
    let bytes = w.total_bytes()?;
    if std::env::var_os("WAVE6_CHECK_ONLY").is_some() {
        for i in 0..n {
            f(i)?;
        }
        eprintln!("PASS {}/{mode}/{regime}", w.name);
        return Ok(());
    }
    for _ in 0..3 {
        for i in 0..n {
            f(i)?;
        }
    }
    let mut times = Vec::with_capacity(20);
    for _ in 0..20 {
        let start = Instant::now();
        for i in 0..n {
            black_box(f(black_box(i))?);
        }
        times.push(start.elapsed().as_nanos() as u64);
    }
    let before = counters();
    for i in 0..n {
        f(i)?;
    }
    let after = counters();
    let mut sorted = times.clone();
    sorted.sort_unstable();
    let p50 = sorted[9] as f64 / n as f64;
    let p95 = sorted[18] as f64 / n as f64;
    let mean = times.iter().sum::<u64>() as f64 / (20 * n) as f64;
    writeln!(
        out,
        "{{\"workload\":\"{}\",\"mode\":\"{mode}\",\"regime\":\"{regime}\",\"documents\":{n},\"payload_bytes\":{bytes},\"median_ns\":{p50},\"p95_ns\":{p95},\"mean_ns\":{mean},\"mb_s\":{},\"allocs_op\":{},\"bytes_op\":{},\"copied_input_bytes\":0,\"samples_pass_ns\":{times:?}}}",
        w.name,
        bytes as f64 * 1000. / (mean * n as f64),
        (after.0 - before.0) as f64 / n as f64,
        (after.1 - before.1) as f64 / n as f64
    )?;
    eprintln!(
        "wave6 {:25} {mode:20} {regime:18} {p50:12.1} ns/doc",
        w.name
    );
    Ok(())
}
fn stress() -> R<Vec<Workload>> {
    let mut out = Vec::new();
    for case in jmax_bench::wave6_stress_cases(32) {
        let (key, kind) = match case.name {
            "reordered-fields" => ("head", ValueKind::Number),
            "sparse-large-subtree" => ("selected", ValueKind::Null),
            "wide-object" => ("field_0", ValueKind::Number),
            _ => ("s", ValueKind::String),
        };
        let graph = Schema {
            name: case.name.into(),
            schema_id: jsonmax::SchemaId(700),
            root: TypeId(0),
            definitions: vec![],
            nodes: vec![
                TypeNode::Object(ObjectShape { nullable: false,
                    properties: vec![Property {
                        name: key.into(),
                        ty: TypeId(1),
                        required: true,
                        slot: Some(0),
                        field_id: jsonmax::FieldId(0),
                    }],
                    reject_unknown: false,
                    min_properties: None,
                    max_properties: None,
                }),
                if kind == ValueKind::Null {
                    TypeNode::Any
                } else {
                    TypeNode::Scalar(ScalarConstraints::new(kind))
                },
            ],
        };
        let schema = jsonmax::endpoint::compile_native_schema(
            &graph,
            jsonmax::CompileLimits::default_limits(),
        )?;
        out.push(Workload {
            name: case.name.into(),
            kind: jmax_bench::WorkloadKind::SmallObject,
            payloads: vec![case.payload; 128],
            schema: None,
            compiled_schema: schema,
            selection: jsonmax::Selection {
                field_ids: vec![jsonmax::FieldId(0)],
                strict_validate_unselected: true,
            },
        });
    }
    Ok(out)
}
fn options() -> jsonmax::ParseOptions {
    let mut o = jsonmax::ParseOptions::default();
    o.limits.reject_unknown_keys = false;
    o
}
fn account(out: &mut impl Write, w: &Workload) -> R<()> {
    let input = &w.payloads[0];
    let d = jsonmax::parse::parse_payload(input.clone(), options())?;
    let legacy = d.to_legacy()?;
    let p = &legacy.parts;
    let legacy_len = p.nodes.len() * std::mem::size_of::<jmax_abi::node::NodeWord>()
        + p.spans.len() * std::mem::size_of::<jmax_abi::span::Span32>()
        + p.containers.len() * std::mem::size_of::<jmax_abi::node::Container32>()
        + p.numbers.len() * std::mem::size_of::<jmax_abi::number::ConvertedNumber>()
        + p.decoded.len()
        + p.node_metadata.len() * std::mem::size_of::<jmax_abi::node::NodeMeta>();
    let legacy_cap = p.nodes.capacity() * std::mem::size_of::<jmax_abi::node::NodeWord>()
        + p.spans.capacity() * std::mem::size_of::<jmax_abi::span::Span32>()
        + p.containers.capacity() * std::mem::size_of::<jmax_abi::node::Container32>()
        + p.numbers.capacity() * std::mem::size_of::<jmax_abi::number::ConvertedNumber>()
        + p.decoded.capacity()
        + p.node_metadata.capacity() * std::mem::size_of::<jmax_abi::node::NodeMeta>();
    let mut scratch = jsonmax::DomScratch::new();
    let warm = jsonmax::parse::parse_payload_with_scratch(input.clone(), options(), &mut scratch)?;
    warm.recycle(&mut scratch);
    writeln!(
        out,
        "{{\"category\":\"storage\",\"workload\":\"{}\",\"input_bytes\":{},\"tape_bytes\":{},\"decoded_bytes\":{},\"warm_tape_capacity_bytes\":{},\"warm_decoded_capacity_bytes\":{},\"dom_stack_capacity\":{},\"duplicate_slots\":{},\"legacy_six_vec_live_bytes\":{legacy_len},\"legacy_six_vec_capacity_bytes\":{legacy_cap},\"pointer_preserved\":{}}}",
        w.name,
        input.len(),
        d.tape().len() * 8,
        d.decoded().len(),
        scratch.tape_capacity() * 8,
        scratch.decoded_capacity(),
        scratch.stack_capacity(),
        scratch.duplicate_capacity(),
        d.raw().as_ptr() == input.as_slice().as_ptr()
    )?;
    Ok(())
}
pub fn run() -> R<()> {
    let path = std::env::args()
        .nth(2)
        .ok_or("--wave6 requires output JSONL path")?;
    let mut out = BufWriter::new(File::create(path)?);
    writeln!(
        out,
        "{{\"category\":\"harness\",\"allocator\":\"{}\",\"warmups\":3,\"iterations\":20,\"seed\":{},\"timing\":\"whole corpus pass averages; drop included; setup excluded\"}}",
        super::active_allocator(),
        jmax_bench::DEFAULT_SEED
    )?;
    let mut suite = jmax_bench::followup::make_suite(CorpusConfig::default())?;
    // The legacy and followup names denote different byte generators; measure both.
    suite.extend(jmax_bench::corpus::make_suite(CorpusConfig::default())?);
    suite.extend(stress()?);
    for w in &suite {
        account(&mut out, w)?;
        let inputs = &w.payloads;
        let schema = &w.compiled_schema;
        let selection = &w.selection;
        let o = options();
        let docs: Vec<_> = inputs
            .iter()
            .map(|p| jsonmax::parse::parse_payload(p.clone(), o))
            .collect::<Result<_, _>>()?;
        // Selected slots must be nonempty and actual result storage is observed.
        if !selection.field_ids.is_empty() {
            for p in inputs {
                let s = jsonmax::endpoint::extract_payload(p.clone(), schema, selection, o)?;
                assert!(
                    s.slots
                        .iter()
                        .any(|s| !matches!(s, jsonmax::SlotValue::Empty))
                );
                black_box(&s);
            }
        }
        bench(&mut out, w, "Parse", "Cold-Owned", |i| {
            black_box(jsonmax::parse::parse_payload(inputs[i].clone(), o)?);
            Ok(())
        })?;
        bench(&mut out, w, "Parse", "Cold-Borrowed", |i| {
            black_box(jsonmax::parse::parse_borrowed(inputs[i].as_slice(), o)?);
            Ok(())
        })?;
        let mut dom = jsonmax::DomScratch::new();
        bench(&mut out, w, "Parse", "Warm-Owned", |i| {
            let d = jsonmax::parse::parse_payload_with_scratch(inputs[i].clone(), o, &mut dom)?;
            black_box(&d);
            d.recycle(&mut dom);
            Ok(())
        })?;
        bench(&mut out, w, "Parse", "Warm-Borrowed", |i| {
            let d = jsonmax::parse::parse_borrowed_with_scratch(inputs[i].as_slice(), o, &mut dom)?;
            black_box(&d);
            d.recycle(&mut dom);
            Ok(())
        })?;
        let mut syntax = jmax_abi::syntax::Scratch::default();
        bench(&mut out, w, "Validate-Syntax", "Warm-Borrowed", |i| {
            black_box(jsonmax::parse::validate_borrowed(
                inputs[i].as_slice(),
                o,
                &mut syntax,
            )?);
            Ok(())
        })?;
        bench(&mut out, w, "Validate-Schema", "Cold-Owned", |i| {
            black_box(jsonmax::endpoint::validate_payload(
                inputs[i].clone(),
                schema,
                o,
            )?);
            Ok(())
        })?;
        let mut vm = jsonmax::endpoint::ValidationScratch::default();
        bench(&mut out, w, "Validate-Schema", "Warm-Owned", |i| {
            black_box(jsonmax::endpoint::validate_payload_with_scratch(
                inputs[i].clone(),
                schema,
                o,
                &mut vm,
            )?);
            Ok(())
        })?;
        bench(&mut out, w, "Validate-Schema", "Warm-Borrowed", |i| {
            black_box(jsonmax::endpoint::validate_borrowed(
                schema,
                inputs[i].as_slice(),
                o,
                &mut vm,
            )?);
            Ok(())
        })?;
        if !selection.field_ids.is_empty() {
            bench(&mut out, w, "Extract-Strict", "Cold-Owned", |i| {
                black_box(jsonmax::endpoint::extract_payload(
                    inputs[i].clone(),
                    schema,
                    selection,
                    o,
                )?);
                Ok(())
            })?;
            bench(&mut out, w, "Extract-Strict", "Cold-Borrowed", |i| {
                black_box(jsonmax::endpoint::extract_borrowed(
                    schema,
                    selection,
                    inputs[i].as_slice(),
                    o,
                )?);
                Ok(())
            })?;
            bench(&mut out, w, "Extract-Strict", "Warm-Owned", |i| {
                black_box(jsonmax::endpoint::extract_payload_with_scratch(
                    &inputs[i], schema, selection, o, &mut vm,
                )?);
                Ok(())
            })?;
            bench(&mut out, w, "Extract-Strict", "Warm-Borrowed", |i| {
                black_box(jsonmax::endpoint::extract_borrowed_with_scratch(
                    schema,
                    selection,
                    inputs[i].as_slice(),
                    o,
                    &mut vm,
                )?);
                Ok(())
            })?;
            bench(&mut out, w, "Extract-Query-Schema", "Cold-Borrowed", |i| {
                black_box(jsonmax::endpoint::extract_query_borrowed(
                    schema,
                    selection,
                    inputs[i].as_slice(),
                    o,
                )?);
                Ok(())
            })?;
        } else {
            writeln!(
                out,
                "{{\"category\":\"capability\",\"workload\":\"{}\",\"operation\":\"Schema Extract\",\"status\":\"N/A: legacy root schema has no capture slots\"}}",
                w.name
            )?;
        }
        let key = if let Some(slot) = schema.program().program().slots.first() {
            let _ = slot;
            match w.name.as_str() {
                "NagoyaAuth" => "user",
                "ExchangeOrder" => "symbol",
                "RpcEnvelope" => "jsonrpc",
                "LargeCatalog" => "version",
                "VectorCoordinates" => "coordinates",
                "DeepNesting" => "root",
                "EscapedUnicode" => "id",
                "long-unescaped-ascii"
                | "long-unescaped-unicode"
                | "escape-dense"
                | "long-backslash-runs" => "s",
                "wide-object" => "field_0",
                "reordered-fields" => "head",
                "sparse-large-subtree" => "selected.id",
                _ => "",
            }
        } else {
            ""
        };
        // Dynamic legacy selectors are obtained from genuine root handles.
        let owned_key: String = if key.is_empty() {
            docs[0]
                .root_node()
                .object()
                .ok()
                .and_then(|obj| obj.iter().next().map(|member| member.key.to_owned()))
                .unwrap_or_else(|| "[0]".into())
        } else {
            key.into()
        };
        let query = jsonmax::query::compile_paths(&[&owned_key])?;
        let qo = jsonmax::QueryOptions {
            mode: jsonmax::QueryMode::Query,
            ..Default::default()
        };
        let pre = jsonmax::query::execute_borrowed(&query, inputs[0].as_slice(), qo)?;
        assert!(
            pre.matches().iter().any(|v| !v.is_empty()),
            "empty query {}: {}",
            w.name,
            owned_key
        );
        bench(&mut out, w, "Extract-Query-Path", "Cold-Borrowed", |i| {
            black_box(jsonmax::query::execute_borrowed(
                &query,
                inputs[i].as_slice(),
                qo,
            )?);
            Ok(())
        })?;
        let mut query_scratch = jsonmax::query::QueryScratch::default();
        bench(&mut out, w, "Extract-Query-Path", "Warm-Borrowed", |i| {
            let result = jsonmax::query::execute_borrowed_with_scratch(
                &query,
                inputs[i].as_slice(),
                qo,
                &mut query_scratch,
            )?;
            black_box(&result);
            query_scratch.recycle_borrowed(result);
            Ok(())
        })?;
        bench(&mut out, w, "Serialize-Staged-Sink", "Cold-Owned", |i| {
            let mut sink = jmax_encode::VecSink::new(32 * 1024 * 1024);
            jsonmax::encode::encode_document_to_sink(&docs[i], &mut sink, Default::default())?;
            black_box(sink);
            Ok(())
        })?;
        let mut output = jsonmax::OutputBuffer::new(32 * 1024 * 1024);
        let mut writer = jmax_encode::WriterScratch::default();
        bench(&mut out, w, "Serialize-Native-Direct", "Warm-Owned", |i| {
            jsonmax::encode::document_to_buffer(&docs[i], &mut output, &mut writer)?;
            black_box(&output);
            Ok(())
        })?;
        out.flush()?;
    }
    ownership(&mut out)?;
    growth(&mut out)?;
    reactive(&mut out, &suite[0])?;
    out.flush()?;
    Ok(())
}
fn ownership(out: &mut impl Write) -> R<()> {
    let bytes = bytes::Bytes::from_static(b"xx{\"id\":7}yy");
    let arc: Arc<[u8]> = Arc::from(bytes.as_ref());
    let generic = Arc::new(bytes.to_vec());
    for (name, payload, original) in [
        ("Bytes", Payload::Bytes(bytes.clone()), bytes.as_ptr()),
        ("Arc-slice", Payload::Shared(arc.clone()), arc.as_ptr()),
        (
            "Arc-T",
            Payload::from_arc_owner(generic.clone())?,
            generic.as_ptr(),
        ),
    ] {
        let before = counters();
        let view = payload.slice(2..10)?;
        let after = counters();
        assert_eq!(view.as_slice().as_ptr(), original.wrapping_add(2));
        let d = jsonmax::parse::parse_payload(view.clone(), options())?;
        assert_eq!(d.raw().as_ptr(), view.as_slice().as_ptr());
        writeln!(
            out,
            "{{\"category\":\"ownership\",\"owner\":\"{name}\",\"pointer_identity\":true,\"view_allocs\":{},\"view_alloc_bytes\":{},\"backing_bytes\":{},\"exact_backing\":{}}}",
            after.0 - before.0,
            after.1 - before.1,
            view.backing_identity().backing_len,
            view.backing_identity().exact_backing
        )?;
    }
    Ok(())
}
fn growth(out: &mut impl Write) -> R<()> {
    let mut scratch = jsonmax::DomScratch::new();
    for size in [1, 32, 32, 1] {
        let case = jmax_bench::wave6_stress_cases(size).remove(0);
        let before = counters();
        let d = jsonmax::parse::parse_payload_with_scratch(case.payload, options(), &mut scratch)?;
        d.recycle(&mut scratch);
        let after = counters();
        writeln!(
            out,
            "{{\"category\":\"growth\",\"scale\":{size},\"allocs\":{},\"bytes\":{},\"tape_capacity\":{}}}",
            after.0 - before.0,
            after.1 - before.1,
            scratch.tape_capacity()
        )?;
    }
    Ok(())
}
fn reactive(out: &mut impl Write, w: &Workload) -> R<()> {
    use jmax_nagoya::{reactive::HostedPool, worker::SchemaValidator, *};
    use std::{
        future::Future,
        pin::pin,
        task::{Context, Poll, Wake, Waker},
    };
    struct WakeCount;
    impl Wake for WakeCount {
        fn wake(self: Arc<Self>) {}
    }
    let waker = Waker::from(Arc::new(WakeCount));
    let mut cx = Context::from_waker(&waker);
    let validator = Arc::new(SchemaValidator::new(
        Arc::new(w.compiled_schema.program().clone()),
        w.selection.clone(),
        options().numeric,
        128_000_000,
    )?);
    let admission = Admission::new(AdmissionLimits {
        max_jobs: 32,
        max_retained_bytes: 64 * 1024 * 1024,
        max_scratch_bytes: 512 * 1024 * 1024,
    })?;
    let (pool, worker) = HostedPool::new(32, 32, 0)?;
    let mut worker = pin!(worker.run());
    let mut sequence = 0u64;
    bench(out, w, "Reactive-Handoff", "Single-Lane-Poll", |i| {
        let input = w.payloads[i].clone();
        let ptr = input.as_slice().as_ptr();
        let job = ParseJob {
            payload: input,
            limits: options().limits,
            cancellation: CancellationToken::new(),
        };
        let lease = admission.try_parse(&job.payload, 16 * 1024 * 1024, 16 * 1024 * 1024)?;
        let mut submit = pin!(pool.submit(validator.clone(), job, lease));
        match submit.as_mut().poll(&mut cx) {
            Poll::Ready(Ok(s)) => assert_eq!(s, sequence),
            _ => return Err("submission did not complete".into()),
        };
        sequence += 1;
        assert!(worker.as_mut().poll(&mut cx).is_pending());
        let mut receive = pin!(pool.recv());
        let result = match receive.as_mut().poll(&mut cx) {
            Poll::Ready(Some(v)) => v,
            _ => return Err("missing reactive completion".into()),
        };
        let value = result.result?;
        assert_eq!(value.value().payload.as_slice().as_ptr(), ptr);
        black_box(value.value());
        drop(value);
        assert_eq!(admission.snapshot(), AdmissionSnapshot::default());
        Ok(())
    })?;
    pool.close();
    assert!(worker.as_mut().poll(&mut cx).is_ready());
    writeln!(
        out,
        "{{\"category\":\"leases\",\"accepted_and_completed\":{sequence},\"final_jobs\":0,\"final_retained_bytes\":0,\"final_scratch_bytes\":0,\"owner_identity\":true,\"scheduler\":\"in-process immediate polling of real reactive futures; no runtime thread or sockets\"}}"
    )?;
    Ok(())
}
