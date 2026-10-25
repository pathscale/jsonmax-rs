use super::*;
use std::fmt::Write as _;
use std::io::Write as _;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 { self.0 ^= self.0 << 13; self.0 ^= self.0 >> 7; self.0 ^= self.0 << 17; self.0 }
    fn below(&mut self, n: usize) -> usize { (self.next() % n as u64) as usize }
}

const SMALL: &[&str] = &[
    r#"{"a":1,"b":[true,false,null],"c":{"d":"e\né🦀","f":-0.5e10}}"#,
    r#"[{"k":"v","k2":"\"\\\/\b\f\n\r\t"},{"k":"v2"},[],{},[[[[{}]]]]]"#,
    r#"{"a":{"a":{"a":{"a":{"a":[1,2,3,{"x":"y"}]}}}},"b":"héllo 世界 🦀","a2":12345678901234567890}"#,
    r#"  {"dup":1, "x": [1, 2.5, -3e-7, 0, -0, 1E+2 ], "s": "tab\tinside"}  "#,
    r#"{"escAkey":"v","escAkey2":"w","n":[1e308,-1e-308,0.0001]}"#,
    r#""just a string""#,
    r#"12345"#,
    r#"[1,[2,[3,[4,[5,[6,[7,[8,[9,[10]]]]]]]]]]"#,
    r#"{"batch_id":"b","tenant":"t","schema_version":1,"request_count":2,"accepted":true,"records":[{"a":"ÿ"}]}"#,
    r#"{"mode":"m","nodeName":"n","numExecutors":3,"quietingDown":false,"jobs":[{"name":"x","url":"y"}]}"#,
];
const BYTES: &[u8] = b"{}[]:,\"\\ \t\n0123456789-+.eEtrufalsn\x00\x1f\x7f\x80\xc3\xa9\xed\xa0\xf0\x9f\xffuA";

fn limits_variants() -> Vec<ParseOptions> {
    let mut v = vec![];
    let base = options();
    v.push(base);
    let mut o = base; o.limits.max_depth = 4; v.push(o);
    let mut o = base; o.limits.max_fields_per_object = 3; v.push(o);
    let mut o = base; o.limits.max_array_items = 3; v.push(o);
    let mut o = base; o.limits.max_string_bytes = 6; v.push(o);
    let mut o = base; o.limits.max_decoded_bytes = 12; v.push(o);
    let mut o = base; o.limits.max_number_digits = 3; v.push(o);
    let mut o = base; o.limits.reject_duplicate_keys = false; v.push(o);
    let mut o = base; o.numeric.reject_negative_zero = false; o.numeric.max_exponent_abs = 5; v.push(o);
    v
}

fn outcome<T: std::fmt::Debug>(out: &mut String, tag: &str, r: Result<T, jsonmax::Error>, show_ok: bool) {
    match r {
        Ok(v) => { if show_ok { let _ = writeln!(out, "{tag} ok {v:?}"); } else { let _ = writeln!(out, "{tag} ok"); } }
        Err(e) => { let _ = writeln!(out, "{tag} err {e:?}"); }
    }
}

fn observe(out: &mut String, input: &[u8], plans: &[Plan], any: &jsonmax::CompiledSchema, query: &jsonmax::CompiledQuery) {
    for (li, o) in limits_variants().into_iter().enumerate() {
        let _ = writeln!(out, "L{li}");
        outcome(out, "validate", jsonmax::parse::validate_borrowed(input, o, &mut jmax_abi::syntax::Scratch::default()).map(|p| format!("{p:?}")), true);
        match jsonmax::parse::parse_borrowed(input, o) {
            Ok(doc) => {
                let _ = writeln!(out, "dom ok {}", doc.node_count());
                if li == 0 {
                    let mut sink = jmax_encode::VecSink::new(jsonmax::DEFAULT_MAX_OUTPUT_BYTES);
                    let r = jsonmax::encode::encode_borrowed_document_streaming(&doc, &mut sink, Default::default());
                    let _ = writeln!(out, "enc {:?} {}", r.is_ok(), String::from_utf8_lossy(&sink.into_vec()));
                    let base = jmax_encode::EncodeLimits::default();
                    let mut variants = vec![];
                    let mut l = base; l.max_container_items = 2; variants.push((l, usize::MAX));
                    let mut l = base; l.max_string_bytes = 4; variants.push((l, usize::MAX));
                    let mut l = base; l.max_number_bytes = 3; variants.push((l, usize::MAX));
                    let mut l = base; l.max_depth = 2; variants.push((l, usize::MAX));
                    let mut l = base; l.max_output_bytes = 10; variants.push((l, usize::MAX));
                    variants.push((base, 10));
                    for (vi, (l, sl)) in variants.into_iter().enumerate() {
                        let mut sink = jmax_encode::VecSink::new(sl);
                        let r = jmax_encode::typed::write_borrowed_document(&doc, &mut sink, l);
                        let _ = writeln!(out, "encv{vi} {:?}", r);
                    }
                }
            }
            Err(e) => { let _ = writeln!(out, "dom err {e:?}"); }
        }
        outcome(out, "any", jsonmax::endpoint::validate_payload(Payload::from_slice(input), any, o).map(|_| ()), false);
        for (pi, p) in plans.iter().enumerate() {
            match jsonmax::endpoint::extract_borrowed(&p.schema, &p.selection, input, o) {
                Ok(s) => { let _ = writeln!(out, "x{pi} ok {:?}", s.slots); }
                Err(e) => { let _ = writeln!(out, "x{pi} err {e:?}"); }
            }
            outcome(out, &format!("xp{pi}"), jsonmax::endpoint::extract_payload(Payload::from_slice(input), &p.schema, &p.selection, o).map(|s| format!("{:?}", s.slots)), true);
        }
        outcome(out, "q", jsonmax::query::execute_borrowed(query, input, jsonmax::QueryOptions { mode: jsonmax::QueryMode::Query, ..Default::default() }).map(|r| format!("{r:?}")), true);
    }
}

pub fn run() -> R<()> {
    let n: usize = std::env::var("DIFF")?.parse()?;
    let dir = std::env::var("JMAX_DATA").unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/../data").into());
    let mut bases: Vec<Vec<u8>> = SMALL.iter().map(|s| s.as_bytes().to_vec()).collect();
    for name in ["github_events", "apache_builds", "endpoint_batch"] {
        let b = fs::read(format!("{dir}/{name}.json"))?;
        bases.push(b);
    }
    let tw = fs::read(format!("{dir}/twitter.json"))?;
    bases.push(tw[..20000].to_vec());
    let plans: Vec<Plan> = ["twitter", "apache_builds", "endpoint_batch", "github_events"].iter().map(|n| plan(n)).collect::<R<_>>()?;
    let any = jsonmax::endpoint::compile_native_schema(
        &Schema { name: "any".into(), schema_id: jsonmax::SchemaId(2), root: TypeId(0), nodes: vec![TypeNode::Any], definitions: vec![] },
        CompileLimits::default_limits(),
    )?;
    let query = jsonmax::query::compile_paths(&["mode"])?;
    let rows_schema = batch_schema(true)?;
    let plain_schema = batch_schema(false)?;
    let mut rows_scratch = jsonmax::endpoint::ValidationScratch::default();
    let mut rng = Rng(0x9e3779b97f4a7c15);
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    for i in 0..n {
        let bi = rng.below(bases.len());
        let mut input = bases[bi].clone();
        let small = bi < SMALL.len();
        let muts = if i % 10 == 0 { 0 } else { 1 + rng.below(3) };
        for _ in 0..muts {
            if input.is_empty() { break; }
            let pos = rng.below(input.len());
            match rng.below(5) {
                0 => input[pos] = BYTES[rng.below(BYTES.len())],
                1 => { input.remove(pos); }
                2 => input.insert(pos, BYTES[rng.below(BYTES.len())]),
                3 => if small || rng.below(4) == 0 { input.truncate(pos) } else { input[pos] = b'"' },
                _ => { let end = (pos + 1 + rng.below(8)).min(input.len()); let chunk = input[pos..end].to_vec(); input.splice(pos..pos, chunk); }
            }
        }
        let mut out = String::new();
        let _ = writeln!(out, "#{i} base{bi} len{}", input.len());
        if bi == 12 || bi == 8 {
            check_rows(&rows_schema, &plain_schema, &mut rows_scratch, &input, options());
        }
        if bi >= SMALL.len() {
            // big inputs: only default limits to keep it fast
            let o = options();
            outcome(&mut out, "validate", jsonmax::parse::validate_borrowed(&input, o, &mut jmax_abi::syntax::Scratch::default()).map(|p| format!("{p:?}")), true);
            outcome(&mut out, "dom", jsonmax::parse::parse_borrowed(&input, o).map(|d| d.node_count()), true);
            outcome(&mut out, "any", jsonmax::endpoint::validate_payload(Payload::from_slice(&input), &any, o).map(|_| ()), false);
            for (pi, p) in plans.iter().enumerate() {
                outcome(&mut out, &format!("x{pi}"), jsonmax::endpoint::extract_borrowed(&p.schema, &p.selection, &input, o).map(|s| format!("{:?}", s.slots)), true);
            }
        } else {
            observe(&mut out, &input, &plans, &any, &query);
        }
        lock.write_all(out.as_bytes())?;
    }
    Ok(())
}

/// A batch schema whose `records` items are open objects. With `rows` the
/// three item fields carry slots (rows API); without, only the array does.
fn batch_schema(rows: bool) -> R<jsonmax::CompiledSchema> {
    let prop = |id: u32, name: &str, ty: u32, required: bool, slot: u32| Property {
        field_id: FieldId(id),
        name: name.into(),
        ty: TypeId(ty),
        required,
        slot: Some(slot),
    };
    let item_slot = |slot: u32| if rows { slot } else { slot + 100 };
    let mut item: Vec<Property> = vec![
        prop(4, "request_id", 4, true, item_slot(3)),
        prop(5, "endpoint", 5, false, item_slot(4)),
        prop(6, "method", 6, false, item_slot(5)),
    ];
    if !rows {
        for p in &mut item {
            p.slot = None;
        }
    }
    let nodes = vec![
        TypeNode::Object(ObjectShape {
            nullable: false,
            properties: vec![
                prop(1, "batch_id", 3, true, 0),
                prop(2, "request_count", 7, true, 1),
                prop(3, "records", 1, true, 2),
            ],
            reject_unknown: false,
            min_properties: None,
            max_properties: None,
        }),
        TypeNode::Array(jmax_schema_ir::ArrayShape { nullable: false, items: TypeId(2), min_items: None, max_items: None }),
        TypeNode::Object(ObjectShape { nullable: false, properties: item, reject_unknown: false, min_properties: None, max_properties: None }),
        TypeNode::Scalar(ScalarConstraints::new(ValueKind::String)),
        TypeNode::Scalar(ScalarConstraints::new(ValueKind::String)),
        TypeNode::Scalar(ScalarConstraints::new(ValueKind::String)),
        TypeNode::Scalar(ScalarConstraints::new(ValueKind::String)),
        TypeNode::Scalar(ScalarConstraints::new(ValueKind::Integer)),
    ];
    Ok(jsonmax::endpoint::compile_native_schema(
        &Schema { name: "batch".into(), schema_id: jsonmax::SchemaId(3), root: TypeId(0), nodes, definitions: vec![] },
        CompileLimits::default_limits(),
    )?)
}

/// The rows API must accept and reject exactly what plain extraction of the
/// same schema (without item slots) does, and stage one row per record.
fn check_rows(rows: &jsonmax::CompiledSchema, plain: &jsonmax::CompiledSchema, scratch: &mut jsonmax::endpoint::ValidationScratch, input: &[u8], o: ParseOptions) {
    let selection = Selection { field_ids: vec![], strict_validate_unselected: true };
    let want = jsonmax::endpoint::extract_borrowed(plain, &selection, input, o).map(|s| s.slots.len());
    let got = jsonmax::endpoint::extract_rows_borrowed_with_scratch(rows, &selection, input, o, scratch);
    match (want, got) {
        (Ok(_), Ok(out)) => assert_eq!(out.groups.len(), 1, "rows group count"),
        (Err(a), Err(b)) => assert_eq!(a.code, b.code, "rows error code on {:?}", String::from_utf8_lossy(input)),
        (a, b) => panic!("rows/plain disagree on {:?}: {:?} vs {:?}", String::from_utf8_lossy(input), a.map(|_| ()), b.map(|_| ())),
    }
}
