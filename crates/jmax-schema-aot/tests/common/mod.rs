#![allow(dead_code)]
use jmax_endpoint_model::native::{NativeSpec, schema_graph_for, validate_native_spec};
use jmax_endpoint_model::registry::build_registry;
use jmax_endpoint_model::SchemaId;
use jmax_endpoint_ron::{ConfigLimits, LoadOptions, load_model, load_native_spec};
use jmax_schema_aot::{ValidatedProgram, compile_graph};

pub const BINANCE: &str = include_str!("../../../../fixtures/native/binance.ron");
pub const BINANCE_NATIVE: &str = include_str!("../../../../fixtures/native/binance.native.ron");
pub const RPC: &str = include_str!("../../../../fixtures/native/rpc.ron");
pub const RPC_NATIVE: &str = include_str!("../../../../fixtures/native/rpc.native.ron");

/// Every fixture root as a compiled reference program.
pub fn fixture_programs() -> Vec<(String, ValidatedProgram)> {
    let mut programs = Vec::new();
    for (model, spec, base) in [(BINANCE, BINANCE_NATIVE, 5000), (RPC, RPC_NATIVE, 6000)] {
        let options = LoadOptions { require_version: false, ..LoadOptions::default() };
        let model = load_model(model, None, &options).unwrap();
        let spec: NativeSpec = load_native_spec(spec, &ConfigLimits::default()).unwrap();
        let registry = build_registry(&model).unwrap();
        validate_native_spec(&registry, &spec).unwrap();
        for (index, root) in spec.roots.iter().enumerate() {
            let schema = schema_graph_for(&registry, &spec, root, SchemaId(base + index as u32)).unwrap();
            programs.push((root.clone(), compile_graph(&schema).unwrap()));
        }
    }
    programs
}

pub fn depth_document(levels: usize, with_snapshot_keys: bool) -> Vec<u8> {
    let level = |i: usize| format!("[\"{}.{:08}\",\"{}.{:08}\"]", i % 90000, i, i % 1000, i * 7);
    let book = |n: usize| (0..n).map(level).collect::<Vec<_>>().join(",");
    if with_snapshot_keys {
        format!("{{\"lastUpdateId\":160,\"bids\":[{}],\"asks\":[{}]}}", book(levels), book(levels))
    } else {
        format!(
            "{{\"e\":\"depthUpdate\",\"E\":123456789,\"s\":\"BNBBTC\",\"U\":157,\"u\":160,\"b\":[{}],\"a\":[{}]}}",
            book(levels), book(levels)
        )
    }
    .into_bytes()
}

pub const AGG_TRADE: &str = r#"{"e":"aggTrade","E":123456789,"s":"BNBBTC","a":12345,"p":"0.00100000","q":"100.00000000","f":100,"l":105,"T":123456785,"m":true,"M":true}"#;
pub const RPC_REQUEST: &str = r#"{"method":"GetAccountBalance","seq":42,"params":{"account_id":"550e8400-e29b-41d4-a716-446655440000"}}"#;

pub fn seeds_for(root: &str) -> Vec<Vec<u8>> {
    match root {
        "AggTrade" => vec![AGG_TRADE.as_bytes().to_vec()],
        "DepthUpdate" => [0, 1, 20, 1000].iter().map(|n| depth_document(*n, false)).collect(),
        "DepthSnapshot" => [0, 1, 20].iter().map(|n| depth_document(*n, true)).collect(),
        _ => vec![RPC_REQUEST.as_bytes().to_vec()],
    }
}

pub struct Rng(pub u64);
impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    pub fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

const BYTES: &[u8] = b"{}[]:,\"\\ \t\n0123456789-+.eEtrufalsn\x00\x1f\x80\xc3\xa9\xed\xf0\xffuA";

/// Byte flips, insertions, deletions, truncation and whitespace runs.
pub fn mutate(input: &[u8], rng: &mut Rng) -> Vec<u8> {
    let mut input = input.to_vec();
    for _ in 0..1 + rng.below(3) {
        if input.is_empty() {
            break;
        }
        let at = rng.below(input.len());
        match rng.below(6) {
            0 => input[at] = BYTES[rng.below(BYTES.len())],
            1 => input.insert(at, BYTES[rng.below(BYTES.len())]),
            2 => {
                input.remove(at);
            }
            3 => input.truncate(at),
            4 => {
                for _ in 0..rng.below(40) {
                    input.insert(at, b' ');
                }
            }
            _ => {
                let end = (at + rng.below(30)).min(input.len());
                let copy = input[at..end].to_vec();
                input.splice(at..at, copy);
            }
        }
    }
    input
}
