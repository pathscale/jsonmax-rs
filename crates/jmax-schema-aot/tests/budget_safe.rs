//! budget_safe must be a true guarantee: real consumption, found by bisecting
//! the budgets, never exceeds the bound, and a safe input never fails on
//! resources under the limits that were judged.

mod common;

use common::{Rng, mutate};
use jmax_abi::number::NumericPolicy;
use jmax_schema_aot::{ValidatedProgram, compile_graph};
use jmax_schema_ir::{
    ArrayShape, ObjectShape, Property, ScalarConstraints, Schema, SchemaId, TypeId, TypeNode,
    ValueKind,
};
use jmax_schema_vm::{ErrorCode, ValidationScratch, VmLimits, budget_bound, budget_safe};
use jmax_schema_vm::direct::validate_borrowed_into;

fn resource(code: ErrorCode) -> bool {
    matches!(code, ErrorCode::BudgetExhausted | ErrorCode::DecodedArenaOverflow | ErrorCode::PayloadTooLarge)
}

fn run(program: &ValidatedProgram, input: &[u8], limits: VmLimits) -> Option<ErrorCode> {
    let numeric = NumericPolicy::strict_default();
    // A fresh scratch: retained capacity from earlier calls counts as scratch.
    match validate_borrowed_into(program, input, limits, numeric, &mut ValidationScratch::default()) {
        Ok(_) => None,
        Err(error) => Some(error.code),
    }
}

/// Smallest budget in 0..=upper for which the run does not fail on resources.
fn smallest(upper: u64, mut fails: impl FnMut(u64) -> bool) -> u64 {
    assert!(!fails(upper), "input already exhausts the generous upper budget");
    let (mut low, mut high) = (0, upper);
    while low < high {
        let mid = low + (high - low) / 2;
        if fails(mid) { low = mid + 1 } else { high = mid }
    }
    low
}

struct Stats {
    inputs: u32,
    safe: u32,
    worst_ratio: f64,
}

fn check(program: &ValidatedProgram, input: &[u8], stats: &mut Stats) {
    let limits = VmLimits::default();
    let bound = budget_bound(program, input.len(), limits).expect("fixture programs are boundable");
    let steps = smallest(bound.steps * 2 + 1_000_000, |max_steps| {
        run(program, input, VmLimits { max_steps, ..limits }).is_some_and(resource)
    });
    let scratch = smallest(u64::from(u32::MAX), |max| {
        let max_scratch_bytes = u32::try_from(max).unwrap();
        run(program, input, VmLimits { max_scratch_bytes, ..limits }).is_some_and(resource)
    });
    assert!(steps <= bound.steps, "steps {steps} exceed bound {} for {} bytes", bound.steps, input.len());
    assert!(
        scratch <= bound.scratch_bytes,
        "scratch {scratch} exceeds bound {} for {} bytes", bound.scratch_bytes, input.len()
    );
    stats.worst_ratio = stats.worst_ratio.max(steps as f64 / bound.steps as f64);
    stats.inputs += 1;
    if budget_safe(program, input.len(), limits) {
        stats.safe += 1;
        assert!(!run(program, input, limits).is_some_and(resource));
    }
}

#[test]
fn measured_consumption_stays_within_the_bound() {
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    let mut stats = Stats { inputs: 0, safe: 0, worst_ratio: 0.0 };
    for (root, program) in common::fixture_programs() {
        for seed in common::seeds_for(&root) {
            check(&program, &seed, &mut stats);
            let rounds = if seed.len() > 4_000 { 6 } else { 120 };
            for _ in 0..rounds {
                check(&program, &mutate(&seed, &mut rng), &mut stats);
            }
        }
    }
    assert!(stats.inputs > 900 && stats.safe > 800, "{} {}", stats.inputs, stats.safe);
    assert!(stats.worst_ratio < 1.0);
}

#[test]
fn dense_decimals_and_deep_nesting_stay_within_the_bound() {
    let programs = common::fixture_programs();
    let depth = &programs.iter().find(|(root, _)| root == "DepthUpdate").unwrap().1;
    let mut stats = Stats { inputs: 0, safe: 0, worst_ratio: 0.0 };
    // The densest possible levels: three byte decimals.
    let levels = vec!["[\"1\",\"2\"]"; 3000].join(",");
    let dense = format!("{{\"e\":\"d\",\"E\":1,\"s\":\"A\",\"U\":1,\"u\":2,\"b\":[{levels}],\"a\":[]}}");
    check(depth, dense.as_bytes(), &mut stats);
    // Deeply nested arrays inside an unrelated position, rejected by the program.
    let nested = format!("{{\"e\":\"d\",\"E\":1,\"s\":\"A\",\"U\":1,\"u\":2,\"b\":{}1{},\"a\":[]}}", "[".repeat(120), "]".repeat(120));
    check(depth, nested.as_bytes(), &mut stats);
    // Escaped and long strings and keys.
    let long = format!("{{\"{}\":1}}", "k\\u00e9".repeat(500));
    check(depth, long.as_bytes(), &mut stats);
    let big = format!("{{\"e\":\"{}\",\"E\":1}}", "\\n".repeat(5000));
    check(depth, big.as_bytes(), &mut stats);
    assert!(stats.worst_ratio < 1.0);
    // Dense decimals make the bound large: 3000 levels are not judged safe at 16M steps,
    // while a typical book is.
    assert!(!budget_safe(depth, 1_000_000, VmLimits::default()));
    assert!(budget_safe(depth, 1_200, VmLimits::default()));
}

fn wide_schema(keys: u32) -> Schema {
    let mut nodes = vec![TypeNode::Any];
    let mut properties = Vec::new();
    for index in 0..keys {
        let kind = [ValueKind::Number, ValueKind::String, ValueKind::Boolean][(index % 3) as usize];
        nodes.push(TypeNode::Scalar(ScalarConstraints::new(kind)));
        properties.push(Property {
            field_id: jmax_abi::schema::FieldId(index + 1),
            name: format!("key{index:03}"),
            ty: TypeId(index + 1),
            required: index % 2 == 0,
            slot: None,
        });
    }
    let list = TypeId(nodes.len() as u32);
    nodes.push(TypeNode::Array(ArrayShape { items: TypeId(0), min_items: None, max_items: None }));
    properties.push(Property {
        field_id: jmax_abi::schema::FieldId(keys + 1), name: "list".into(), ty: list, required: false, slot: None,
    });
    nodes[0] = TypeNode::Object(ObjectShape { properties, reject_unknown: true, min_properties: None, max_properties: None });
    // The array items must be a real type: point them at a number scalar.
    let number = TypeId(nodes.len() as u32);
    nodes.push(TypeNode::Scalar(ScalarConstraints::new(ValueKind::Number)));
    nodes[list.0 as usize] = TypeNode::Array(ArrayShape { items: number, min_items: None, max_items: None });
    Schema { name: format!("wide{keys}"), schema_id: SchemaId(9000 + keys), root: TypeId(0), nodes, definitions: vec![] }
}

#[test]
fn wide_and_perfect_hash_tables_stay_within_the_bound() {
    let mut rng = Rng(77);
    let mut stats = Stats { inputs: 0, safe: 0, worst_ratio: 0.0 };
    for keys in [3u32, 8, 9, 40, 70, 200] {
        let program = compile_graph(&wide_schema(keys)).unwrap();
        let mut doc = String::from("{");
        for index in (0..keys).step_by(2) {
            let value = ["1.5e3", "\"s\\u00e9\"", "true"][(index % 3) as usize];
            doc.push_str(&format!("\"key{index:03}\":{value},"));
        }
        doc.push_str("\"list\":[1,2.5,-3e-2,0]}");
        let seed = doc.into_bytes();
        check(&program, &seed, &mut stats);
        for _ in 0..80 {
            check(&program, &mutate(&seed, &mut rng), &mut stats);
        }
    }
    assert!(stats.worst_ratio < 1.0);
}

#[test]
fn unbounded_programs_and_tight_limits_are_not_safe() {
    let mut open = wide_schema(3);
    if let TypeNode::Object(object) = &mut open.nodes[0] {
        object.reject_unknown = false;
    }
    let program = compile_graph(&open).unwrap();
    assert!(budget_bound(&program, 10, VmLimits::default()).is_none());
    assert!(!budget_safe(&program, 10, VmLimits::default()));

    let mut pattern = wide_schema(3);
    if let TypeNode::Scalar(c) = &mut pattern.nodes[2] {
        c.pattern = Some("^a+$".into());
    }
    let program = compile_graph(&pattern).unwrap();
    assert!(budget_bound(&program, 10, VmLimits::default()).is_none());

    let programs = common::fixture_programs();
    let trade = &programs.iter().find(|(root, _)| root == "AggTrade").unwrap().1;
    let limits = VmLimits::default();
    assert!(budget_safe(trade, 200, limits));
    assert!(!budget_safe(trade, 200, VmLimits { max_steps: 100, ..limits }));
    assert!(!budget_safe(trade, 200, VmLimits { max_scratch_bytes: 100, ..limits }));
    let mut small = limits;
    small.syntax.max_payload_bytes = 100;
    assert!(!budget_safe(trade, 200, small));
    let mut small = limits;
    small.syntax.max_decoded_bytes = 100;
    assert!(!budget_safe(trade, 200, small));
    assert!(!budget_safe(trade, usize::MAX, limits));
}

/// `check` against the bound for a known number of decimals.
fn check_counted(program: &ValidatedProgram, input: &[u8], decimals: u64, stats: &mut Stats) {
    let limits = VmLimits::default();
    let bound = jmax_schema_vm::budget_bound_counted(program, input.len(), decimals, limits)
        .expect("fixture programs are boundable");
    let steps = smallest(bound.steps * 2 + 1_000_000, |max_steps| {
        run(program, input, VmLimits { max_steps, max_scratch_bytes: u32::MAX, ..limits }).is_some_and(resource)
    });
    let scratch = smallest(u64::from(u32::MAX), |max| {
        let max_scratch_bytes = u32::try_from(max).unwrap();
        run(program, input, VmLimits { max_scratch_bytes, max_steps: u64::MAX, ..limits }).is_some_and(resource)
    });
    assert!(steps <= bound.steps, "steps {steps} exceed counted bound {} for {} bytes, {decimals} decimals", bound.steps, input.len());
    assert!(scratch <= bound.scratch_bytes, "scratch {scratch} exceeds {}", bound.scratch_bytes);
    stats.worst_ratio = stats.worst_ratio.max(steps as f64 / bound.steps as f64);
    stats.inputs += 1;
    if jmax_schema_vm::budget_safe_counted(program, input.len(), decimals, limits) {
        stats.safe += 1;
        assert!(!run(program, input, limits).is_some_and(resource));
    }
}

#[test]
fn counted_bound_covers_large_books_and_long_decimals() {
    let programs = common::fixture_programs();
    let program = |name: &str| &programs.iter().find(|(root, _)| root == name).unwrap().1;
    let (update, snapshot) = (program("DepthUpdate"), program("DepthSnapshot"));
    let mut stats = Stats { inputs: 0, safe: 0, worst_ratio: 0.0 };
    let mut rng = Rng(0xdec1_0a1c_0ffe_e001);

    // Books of known size: four decimals per level pair (two per side level).
    for levels in [0usize, 1, 20, 100, 1000, 3000] {
        let doc = common::depth_document(levels, false);
        check_counted(update, &doc, 4 * levels as u64, &mut stats);
        let snap = common::depth_document(levels, true);
        check_counted(snapshot, &snap, 4 * levels as u64, &mut stats);
    }
    // The goal: a 1000 level book under the default limits is judged safe once its
    // decimals are counted, while the length alone cannot vouch for it.
    let book = common::depth_document(1000, true);
    assert!(book.len() > 60_000);
    assert!(!budget_safe(snapshot, book.len(), VmLimits::default()));
    assert!(jmax_schema_vm::budget_safe_counted(snapshot, book.len(), 4000, VmLimits::default()));
    // But not a book of the densest decimals whose count would need more steps than allowed.
    assert!(!jmax_schema_vm::budget_safe_counted(snapshot, book.len(), 40_000, VmLimits::default()));

    // The densest decimals: one digit, two per level.
    let dense = format!("{{\"lastUpdateId\":1,\"bids\":[{}],\"asks\":[]}}", vec!["[\"1\",\"2\"]"; 2500].join(","));
    check_counted(snapshot, dense.as_bytes(), 5000, &mut stats);
    // Long, odd and malformed decimals: many digits, exponents, signs, escapes.
    let odd = [
        "123456789012345678901234567890.123456789012345678901234567890",
        "-0.00000000000000000000000000000000000001",
        "1e400", "1E-400", "-1.5e3", "00.1", "1.", ".5", "+1", "1\\u002e5", "\\u0031\\u0032",
        "99999999999999999999999999999999999999.99999999999999999999",
        "0.000000000000000000000000000000000000000000000000000000000000000001",
    ];
    for text in odd {
        for repeat in [1usize, 40, 400] {
            let level = format!("[\"{text}\",\"{text}\"]");
            let levels = vec![level; repeat].join(",");
            let doc = format!("{{\"lastUpdateId\":1,\"bids\":[{levels}],\"asks\":[{levels}]}}");
            check_counted(snapshot, doc.as_bytes(), 4 * repeat as u64, &mut stats);
        }
    }
    // Mutations of large books, with the count of the seed (an upper bound for most
    // mutants; a mutant with more decimals than the seed cannot arise from one byte
    // change except by splitting a string, and every mutant is checked against its
    // own quote count as well).
    for levels in [20usize, 1000] {
        let seed = common::depth_document(levels, true);
        let seed_decimals = 4 * levels as u64;
        for _ in 0..if levels > 100 { 8 } else { 200 } {
            let mutant = mutate(&seed, &mut rng);
            let quotes = mutant.iter().filter(|byte| **byte == b'"').count() as u64;
            check_counted(snapshot, &mutant, seed_decimals.max(quotes / 2), &mut stats);
        }
    }
    assert!(stats.worst_ratio < 1.0);
    eprintln!("counted bound: worst measured/bound {:.3} over {} inputs, {} safe", stats.worst_ratio, stats.inputs, stats.safe);
    assert!(stats.safe > 20, "{} of {}", stats.safe, stats.inputs);
}
